# Phase Task List: phase-01-daemon-shell-selection

## Metadata

- workflow_id: wf-cmd-shell-support-20260522-91e9
- phase_id: phase-01-daemon-shell-selection
- phase_goal: Add a Windows shell preference parser and wire it into session creation while preserving PowerShell as the default.
- source_plan: docs/plans/wf-cmd-shell-support-20260522-91e9.md
- self_review_verdict: pass

## Tasks

### task-01-add-shell-parser

- objective: Add a Windows-only shell selection helper that maps `QSCREEN_WINDOWS_SHELL` values to supported shell executable paths.
- files_or_artifacts:
  - `crates/qscreen-daemon/src/session.rs`
- ordered_steps:
  1. Inspect the existing `default_shell_command()` implementation and its call sites in `session.rs`.
  2. Introduce a pure helper for resolving a shell preference string so tests do not need to mutate process environment.
  3. Preserve the existing PowerShell path for unset or empty values.
  4. Resolve `cmd` and `cmd.exe` to `C:\Windows\System32\cmd.exe`.
  5. Resolve `powershell` and `powershell.exe` to `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe`.
  6. Return a clear error for unsupported values instead of falling back silently.
- verification:
  - Review the helper against all supported values from the phase plan.
  - Confirm the helper is compiled only where Windows shell selection is relevant, or otherwise remains platform-safe.
- done_definition:
  - Shell preference parsing is deterministic and covers unset, empty, `cmd`, `cmd.exe`, `powershell`, `powershell.exe`, and unknown values.
  - Unknown values include the rejected value in the error message or otherwise give enough detail to fix configuration.
- rollback_or_retry_note:
  - If return-type propagation becomes too broad, keep parsing isolated and only convert to `anyhow::Result` at the smallest existing boundary that can prevent session creation.

### task-02-wire-session-creation

- objective: Wire shell resolution into daemon session creation so invalid configuration prevents session startup.
- files_or_artifacts:
  - `crates/qscreen-daemon/src/session.rs`
- ordered_steps:
  1. Update `default_shell_command()` to read `QSCREEN_WINDOWS_SHELL`.
  2. Change `default_shell_command()` to return an error-capable type if needed.
  3. Propagate the error through `Session::new()` or the existing creation path that obtains the default shell command.
  4. Keep PowerShell as the default behavior when the environment variable is unset or empty.
  5. Confirm no protocol message or schema changes are introduced.
- verification:
  - Build or test `qscreen-daemon` enough to catch type propagation errors.
  - Inspect diff to confirm no changes under `crates/qscreen-protocol`.
- done_definition:
  - Session creation uses the selected shell command.
  - Unsupported `QSCREEN_WINDOWS_SHELL` values fail before a session is started.
  - Existing default behavior is unchanged when the env var is absent or blank.
- rollback_or_retry_note:
  - Roll back by restoring the fixed PowerShell command path and removing env parsing.

### task-03-add-focused-tests

- objective: Add focused unit tests for Windows shell preference behavior without requiring live ConPTY or named pipes.
- files_or_artifacts:
  - `crates/qscreen-daemon/src/session.rs`
- ordered_steps:
  1. Add tests near the shell selection helper in `session.rs`.
  2. Cover unset-equivalent and empty preference values.
  3. Cover `cmd` and `cmd.exe`.
  4. Cover `powershell` and `powershell.exe`.
  5. Cover an unsupported value and assert it returns an error.
  6. Avoid tests that require a running daemon, ConPTY, or Windows named-pipe runtime.
- verification:
  - Run targeted daemon tests if available.
  - Otherwise run `cargo test -p qscreen-daemon`.
  - Confirm non-Windows tests do not require Windows runtime.
- done_definition:
  - Unit tests cover all phase deliverable cases.
  - Tests avoid global environment mutation or isolate it enough to prevent parallel flakiness.
- rollback_or_retry_note:
  - If environment mutation is unavoidable, serialize or scope it carefully; prefer retrying with a pure parser before accepting env-based tests.

## Self Review

- Coverage: pass. The tasks cover parser behavior, session wiring, hard errors for unsupported values, default PowerShell behavior, cmd aliases, PowerShell aliases, tests, and protocol non-change confirmation.
- Ordering: pass. Parser comes first, session integration second, tests and verification third.
- Verification: pass. Each task has specific review checks or command checks, and the phase-level daemon test command is included.
- Dependency assumptions: pass. The task list assumes only `crates/qscreen-daemon/src/session.rs` is needed unless implementation discovers an existing creation boundary elsewhere.
- Verdict: pass. Deliverable fully covers the phase plan and can be executed directly.
