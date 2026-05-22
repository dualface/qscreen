# 阶段任务清单: phase-04-tests-docs-validation

## 状态

- workflow_id: wf-multi-attach-clients-20260522-36f5
- phase_id: phase-04-tests-docs-validation
- phase_plan_path: /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/docs/plans/wf-multi-attach-clients-20260522-36f5.md
- self_review_verdict: pass
- owner: codex-worker

## 阶段目标

Complete required automated coverage, documentation updates, and build validation for host and Windows target after protocol, daemon multi-client, and focus/input/resize semantics are implemented.

## 任务

### 任务 01: task-01-update-readme-docs

- objective: Document multi-client attach behavior and focus/input based latest-client sizing at a high level in both README files.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/README.md
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/README_CN.md
- ordered_steps:
  - Review existing README sections that describe attach, session behavior, and terminal sizing.
  - Add or update concise English documentation for multiple clients attached to one session, broadcast output, independent detach, and latest active client sizing by focus or input.
  - Add or update matching Chinese documentation with the same behavior claims.
  - Keep documentation aligned with implemented behavior from phases 01 through 03 and avoid promising unavailable runtime support outside Windows daemon mode.
- verification:
  - Review both README files for matching English and Chinese behavior coverage.
  - Confirm docs mention multiple attach behavior and focus/input based latest-client sizing.
- done_definition: README.md and README_CN.md describe the implemented multi-attach and active-client sizing semantics without contradicting platform limits.
- rollback_or_retry_note: Revert only the documentation edits if they describe behavior not actually implemented; then rewrite against observed code behavior.

### 任务 02: task-02-add-coverage-tests

- objective: Add focused automated coverage for practical remaining multi-client behavior, list attached semantics, all-client notifications, handler attach/detach paths, and focus handling.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-protocol/src
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-client/src
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-shared/src
- ordered_steps:
  - Identify existing inline tests closest to session attachment, daemon handler command processing, client list output, and protocol command behavior.
  - Add or extend tests for `qscn ls` boolean attached semantics where the client code exposes testable formatting or response handling.
  - Add or extend session tests proving exit or kill notification reaches all attached clients where practical without live ConPTY.
  - Add or extend daemon handler tests for second attach, explicit detach, and focus command using in-memory streams if existing handler seams allow it.
  - If a deliverable is not practical because it requires live Windows ConPTY or unexposed IPC seams, record the limitation in the test or validation notes artifact instead of adding brittle tests.
- verification:
  - `cargo test --workspace`
  - Review skipped or documented limitations, if any, and confirm each is tied to a practical testability constraint.
- done_definition: Practical automated tests cover the remaining phase deliverables, and any untestable runtime-only behavior has an explicit validation note.
- rollback_or_retry_note: If a new test depends on timing or platform-specific runtime behavior, replace it with a smaller unit test around deterministic session, handler, or formatting logic.

### 任务 03: task-03-record-manual-validation

- objective: Record manual validation notes for multi-terminal attach and size ownership when Windows daemon runtime validation is performed, or document why it was not performed.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/docs
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/docs/tasks/phase-04-tests-docs-validation.md
- ordered_steps:
  - Locate the project docs location used for workflow validation notes, or create a focused note under docs if no convention exists.
  - Record manual multi-terminal attach checklist results when Windows daemon runtime is available.
  - Record manual size ownership checklist results for focus, input, active resize, and inactive resize when Windows daemon runtime is available.
  - If Windows daemon runtime is unavailable, record that blocker with host, target, command attempted if any, and remaining manual validation needed.
- verification:
  - Manual note includes either performed results or a concrete blocker for each required checklist.
  - Notes distinguish automated host validation from Windows runtime validation.
- done_definition: Manual validation artifact exists or the task list is updated with concrete unperformed-runtime blockers, covering both multi-terminal attach and size ownership.
- rollback_or_retry_note: If manual notes are incomplete, rerun the checklist on Windows or replace vague notes with exact unavailable-runtime blockers.

### 任务 04: task-04-run-validation-suite

- objective: Run formatting, workspace tests, clippy, and Windows target check, then capture any true blockers.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/docs/tasks/phase-04-tests-docs-validation.md
- ordered_steps:
  - Run `cargo fmt --all`.
  - Run `cargo test --workspace`.
  - Run `cargo clippy --workspace --all-targets`.
  - Run `cargo check --workspace --target x86_64-pc-windows-gnu`.
  - Fix failures introduced by this phase when they are in scope.
  - Document environment or target blockers that prevent a required command from completing.
- verification:
  - `cargo fmt --all`
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets`
  - `cargo check --workspace --target x86_64-pc-windows-gnu`
- done_definition: All required validation commands pass, or any non-code blocker is explicitly documented with enough detail for retry.
- rollback_or_retry_note: If validation fails because of a phase change, revert or narrow that change and rerun the failing command before preserving docs-only updates.

## 自审

- deliverables_covered: yes
- dependency_order_valid: yes
- blockers_or_assumptions: Phase execution assumes phases 01, 02, and 03 are already implemented; Windows runtime manual validation may be unavailable on the host and must then be documented as a blocker.
- ready_for_execution: yes

## Override 记录

- none
