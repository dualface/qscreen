# Phase Task List: Document Windows Shell Configuration

## Metadata

- workflow_id: wf-cmd-shell-support-20260522-91e9
- phase_id: phase-02-document-windows-shell-config
- phase_goal: Update user documentation for the Windows default shell and the `cmd.exe` opt-in environment variable.
- depends_on:
  - phase-01-daemon-shell-selection
- self_review_verdict: pass

## Tasks

### task-01-update-english-readme

- objective: Update English documentation to describe the Windows daemon shell default and supported `cmd.exe` opt-in values.
- files_or_artifacts:
  - `README.md`
- ordered_steps:
  1. Locate the existing Windows usage, configuration, or daemon behavior section in `README.md`.
  2. Add or update text stating that Windows sessions default to `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe`.
  3. Document that setting `QSCREEN_WINDOWS_SHELL=cmd` or `QSCREEN_WINDOWS_SHELL=cmd.exe` makes the daemon start `C:\Windows\System32\cmd.exe`.
  4. Document that `powershell` and `powershell.exe` keep the default PowerShell behavior when used as explicit values.
  5. State that unsupported `QSCREEN_WINDOWS_SHELL` values produce an error and prevent session creation.
  6. Keep wording scoped to daemon-wide environment configuration; do not imply per-session CLI selection or arbitrary shell command support.
- verification:
  - Review `README.md` and confirm it mentions PowerShell default, `cmd` and `cmd.exe` aliases, PowerShell aliases, and unsupported-value error behavior.
  - Confirm the English docs do not mention protocol changes, per-session shell flags, or arbitrary executable paths.
- done_definition: English README accurately documents the phase-01 shell selection behavior without overstating support.
- rollback_or_retry_note: Revert only the `README.md` documentation edits if wording proves inaccurate; daemon implementation remains unchanged.

### task-02-update-chinese-readme

- objective: Mirror the Windows shell configuration behavior in Chinese documentation.
- files_or_artifacts:
  - `README_CN.md`
- ordered_steps:
  1. Locate the corresponding Windows usage, configuration, or daemon behavior section in `README_CN.md`.
  2. Add or update Chinese text stating that Windows sessions default to `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe`.
  3. Document that `QSCREEN_WINDOWS_SHELL=cmd` and `QSCREEN_WINDOWS_SHELL=cmd.exe` select `C:\Windows\System32\cmd.exe`.
  4. Document that `powershell` and `powershell.exe` explicitly select the default PowerShell behavior.
  5. State that unsupported values produce an error and prevent session creation.
  6. Keep scope aligned with the English README and avoid implying per-session CLI selection or arbitrary command support.
- verification:
  - Review `README_CN.md` and confirm it mirrors all supported values and error behavior from `README.md`.
  - Confirm the Chinese docs remain limited to env-based daemon configuration.
- done_definition: Chinese README contains the same user-facing behavior and limits as the English README.
- rollback_or_retry_note: Revert only the `README_CN.md` documentation edits if wording proves inaccurate; daemon implementation remains unchanged.

### task-03-cross-check-doc-consistency

- objective: Verify both README updates match phase-01 behavior and each other.
- files_or_artifacts:
  - `README.md`
  - `README_CN.md`
- ordered_steps:
  1. Compare English and Chinese shell configuration sections for equivalent behavior.
  2. Confirm both documents say the default is PowerShell.
  3. Confirm both documents list `cmd`, `cmd.exe`, `powershell`, and `powershell.exe` as the only supported values.
  4. Confirm both documents say unsupported values return an error and prevent session creation.
  5. Confirm neither document describes per-session CLI shell selection, arbitrary command paths, protocol changes, or non-Windows behavior changes.
- verification:
  - Manual documentation review of both README files against the phase plan deliverables.
  - Optional search check: `rg "QSCREEN_WINDOWS_SHELL|cmd\\.exe|PowerShell|powershell" README.md README_CN.md`.
- done_definition: Documentation deliverables are fully covered, internally consistent, and executable by phase-03 verification.
- rollback_or_retry_note: If consistency review finds a mismatch, revise the smaller or less precise README section and repeat this task.

## Self Review

- Coverage: pass. Tasks cover English README, Chinese README, supported values, default behavior, and unsupported-value error behavior.
- Ordering: pass. English documentation is drafted first, Chinese documentation mirrors it, then both are cross-checked.
- Verification: pass. Each task has concrete review checks and the final task validates phase constraints.
- Dependencies: pass. Task list assumes phase-01 behavior exists before docs are executed, matching `depends_on`.
- Blockers: none.
- Verdict: pass. Deliverable fully covers phase-02 and can be executed directly.
