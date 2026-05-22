# Phase Plan List: Windows cmd.exe shell support

## Metadata

- workflow_id: wf-cmd-shell-support-20260522-91e9
- source_spec: docs/specs/wf-cmd-shell-support-20260522-91e9.md
- status: ready_for_task_synthesis
- generated_at: 2026-05-22T05:36:51Z
- self_review_verdict: pass

## Plan Summary

Implement Windows-only `QSCREEN_WINDOWS_SHELL` selection in the daemon, document the supported values, then run formatting and available workspace checks. The unknown-value strategy is fixed as hard error so misspelled configuration does not silently start PowerShell.

## Phases

### phase-01-daemon-shell-selection

- title: Daemon Windows shell selection
- goal: Add a Windows shell preference parser and wire it into session creation while preserving PowerShell as the default.
- depends_on: []
- deliverables:
  - `default_shell_command()` Windows path reads `QSCREEN_WINDOWS_SHELL`.
  - Unset or empty value resolves to `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe`.
  - `cmd` and `cmd.exe` resolve to `C:\Windows\System32\cmd.exe`.
  - `powershell` and `powershell.exe` resolve to the existing PowerShell path.
  - Unsupported values return a clear error and prevent session creation.
  - Focused unit tests cover unset, empty, cmd aliases, PowerShell aliases, and unknown value behavior without requiring live ConPTY or named pipes.
- likely files or artifacts:
  - `crates/qscreen-daemon/src/session.rs`
- verification:
  - Run targeted daemon tests if available, otherwise `cargo test -p qscreen-daemon`.
  - Confirm non-Windows tests do not require Windows runtime.
  - Confirm no `qscreen-protocol` schema changes.
- risks_and_rollback:
  - Risk: changing `default_shell_command()` return type may require propagating `anyhow::Result` through `Session::new()`.
  - Risk: tests that mutate environment can be flaky if run in parallel; isolate parsing into pure helper functions where practical.
  - Rollback: remove env parsing and restore the previous fixed PowerShell command path.
- self_review_verdict: pass

### phase-02-document-windows-shell-config

- title: Document Windows shell configuration
- goal: Update user documentation for the Windows default shell and the `cmd.exe` opt-in environment variable.
- depends_on:
  - phase-01-daemon-shell-selection
- deliverables:
  - English README explains that Windows defaults to PowerShell.
  - English README documents `QSCREEN_WINDOWS_SHELL=cmd` or `QSCREEN_WINDOWS_SHELL=cmd.exe`.
  - Chinese README mirrors the same behavior and supported values.
  - Documentation notes unsupported values produce an error, matching the implementation strategy.
- likely files or artifacts:
  - `README.md`
  - `README_CN.md`
- verification:
  - Review docs for consistency with phase-01 behavior.
  - Confirm docs do not imply per-session CLI selection or arbitrary command support.
- risks_and_rollback:
  - Risk: docs may overstate support beyond the env-based daemon configuration.
  - Rollback: revert README changes only; daemon behavior remains covered by phase 01.
- self_review_verdict: pass

### phase-03-workspace-verification

- title: Format and workspace checks
- goal: Run required formatting and test commands, plus the Windows target check when available, and record any platform limitations.
- depends_on:
  - phase-01-daemon-shell-selection
  - phase-02-document-windows-shell-config
- deliverables:
  - `cargo fmt --all` run and passing.
  - `cargo test --workspace` run and passing, or failure recorded with concrete platform/toolchain reason.
  - `cargo check --workspace --target x86_64-pc-windows-gnu` attempted when target/toolchain is available, with result recorded.
  - Final implementation notes include any Windows-specific manual testing not performed on the current host.
- likely files or artifacts:
  - Verification output in worker result or implementation report.
- verification:
  - Inspect command exits and relevant error messages.
  - Confirm no unexpected protocol or CLI changes appeared in the diff.
- risks_and_rollback:
  - Risk: Windows target check may fail on non-Windows host due missing target or linker support; record exact reason rather than treating it as implementation failure.
  - Rollback: no code rollback expected from this phase unless verification exposes a regression in earlier phases.
- self_review_verdict: pass

## Self Review

- Coverage: pass. The phases cover daemon behavior, user docs, and required verification from the approved spec.
- Ordering: pass. Implementation precedes documentation so docs can mirror the chosen unknown-value strategy; verification runs last.
- Dependencies: pass. Each phase has explicit `depends_on` values.
- Deliverables: pass. Each phase produces concrete code, docs, or verification artifacts.
- Verification: pass. Tests focus on shell selection without requiring ConPTY integration; workspace checks match the spec.
- Tasking readiness: pass. Later task synthesis does not need to infer phase boundaries or unknown-value behavior.
