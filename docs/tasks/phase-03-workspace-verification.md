# 阶段任务清单: phase-03-workspace-verification

## 状态

- workflow_id: wf-cmd-shell-support-20260522-91e9
- phase_id: phase-03-workspace-verification
- phase_plan_path: docs/plans/wf-cmd-shell-support-20260522-91e9.md
- self_review_verdict: pass
- owner: codex-worker

## 阶段目标

Run required formatting and workspace verification after daemon shell selection and documentation are complete, attempt the Windows target check when the toolchain supports it, and record any platform limits or manual Windows testing gaps.

## 任务

### 任务 01: task-01-run-formatting

- objective: Format the full Rust workspace so implementation and documentation-adjacent code changes follow repository style.
- files_or_artifacts:
  - workspace Rust sources formatted by `cargo fmt --all`
  - verification output in the implementation report or worker result
- ordered_steps:
  - Confirm phase dependencies `phase-01-daemon-shell-selection` and `phase-02-document-windows-shell-config` have already been applied in the current worktree.
  - Run `cargo fmt --all` from the workspace root.
  - If formatting changes files, keep them as part of the verification phase output.
- verification:
  - `cargo fmt --all` exits with code 0.
  - Optional check: rerun `cargo fmt --all -- --check` if the executor wants an explicit no-diff format assertion.
- done_definition: Formatting command completes successfully and any formatter edits are left in the worktree for final review.
- rollback_or_retry_note: If formatting fails, record the exact rustfmt/toolchain error, fix toolchain availability if possible, then rerun; no code rollback is expected unless formatter output exposes invalid syntax from earlier phases.

### 任务 02: task-02-run-workspace-tests

- objective: Run the available workspace tests and capture pass/fail status with concrete failure reasons.
- files_or_artifacts:
  - verification output in the implementation report or worker result
- ordered_steps:
  - Run `cargo test --workspace` from the workspace root.
  - Inspect the exit code and the first relevant compiler, test, or platform error if the command fails.
  - Record whether any failure is an implementation regression or a host/toolchain limitation.
- verification:
  - `cargo test --workspace` exits with code 0, or the failure is recorded with concrete platform/toolchain reason and relevant error text.
  - Confirm non-Windows test execution did not require a live ConPTY daemon.
- done_definition: Workspace test result is known, with pass status or actionable failure context recorded.
- rollback_or_retry_note: If tests fail due an implementation regression, return to the responsible earlier phase changes and fix before rerunning; if failure is environmental, record it and continue to the Windows target check.

### 任务 03: task-03-check-windows-target-and-diff

- objective: Attempt the Windows GNU target check when available, then review the final diff for unintended protocol or CLI changes and note manual Windows testing limitations.
- files_or_artifacts:
  - verification output in the implementation report or worker result
  - final diff review notes
- ordered_steps:
  - Check whether the `x86_64-pc-windows-gnu` target/toolchain is installed or otherwise usable by Cargo.
  - Attempt `cargo check --workspace --target x86_64-pc-windows-gnu` when the target/toolchain is available.
  - If the target check cannot run or fails because of missing target, linker, or host support, record the exact reason.
  - Inspect the final diff and confirm no unexpected `qscreen-protocol` schema change or unrelated CLI behavior change is present.
  - Record that live Windows ConPTY/manual daemon testing was or was not performed on the current host.
- verification:
  - Windows target check exits with code 0, or its platform/toolchain limitation is recorded with concrete error text.
  - Diff review confirms no unexpected protocol or CLI changes.
  - Final notes include Windows-specific manual testing status.
- done_definition: Target check outcome, diff review, and manual Windows testing status are all captured for final reporting.
- rollback_or_retry_note: If target check reveals a real compile regression, fix the earlier implementation and rerun all phase verification; if the issue is missing local Windows GNU support, keep the implementation and report the limitation.

## 自审

- deliverables_covered: yes
- dependency_order_valid: yes
- blockers_or_assumptions: assumes phase-01-daemon-shell-selection and phase-02-document-windows-shell-config are complete before these verification tasks execute
- ready_for_execution: yes

## Override 记录

- none
