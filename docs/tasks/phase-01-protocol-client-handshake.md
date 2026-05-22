# 阶段任务清单: phase-01-protocol-client-handshake

## 状态

- workflow_id: wf-multi-attach-clients-20260522-36f5
- phase_id: phase-01-protocol-client-handshake
- phase_plan_path: /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/docs/plans/wf-multi-attach-clients-20260522-36f5.md
- self_review_verdict: pass
- owner: codex-worker

## 阶段目标

Add the `Focus` protocol command and make `Attach` require a valid terminal size, then update the client attach path to send its initial size in `Attach` and stop sending the immediate post-attach `Resize`.

## 任务

### 任务 01: task-01-add-focus-command

- objective: Add `Command::Focus` to the wire protocol with stable JSON serialization and deserialization.
- files_or_artifacts:
  - crates/qscreen-protocol/src
- ordered_steps:
  - Find the protocol command enum and its serde representation.
  - Add the `Focus` variant without changing existing command names or payload fields.
  - Ensure JSON encoding and decoding use the same enum style as existing commands.
- verification:
  - Add or update a protocol unit test proving `Command::Focus` round trips through JSON.
  - Run `cargo test -p qscreen-protocol`.
- done_definition: `Command::Focus` compiles, serializes, deserializes, and has a passing round-trip test.
- rollback_or_retry_note: If serialization shape differs from existing commands, retry by matching the current enum tagging pattern instead of introducing a new JSON format.

### 任务 02: task-02-require-attach-size

- objective: Change `Attach` validation so missing, zero, or otherwise invalid `width,height` is rejected.
- files_or_artifacts:
  - crates/qscreen-protocol/src
  - crates/qscreen-daemon/src/lib.rs
- ordered_steps:
  - Locate `Attach` request fields and validation helpers.
  - Require both `width` and `height` to be present.
  - Reuse existing terminal-size bounds or validation constants where available.
  - Preserve existing validation for session names and other attach fields.
  - Wire the attach-size validation helper into daemon `handle_attach` so invalid attach requests are rejected at runtime before session attach.
- verification:
  - Add protocol tests for missing width, missing height, zero width, zero height, and valid size.
  - Run `cargo test -p qscreen-protocol`.
  - Run `cargo test -p qscreen-daemon` after the daemon handler import/call change.
- done_definition: Invalid `Attach` sizes are rejected, valid sizes are accepted, and existing protocol validation behavior remains stable.
- rollback_or_retry_note: If old tests assumed missing size was accepted, update those tests to the new contract rather than weakening validation.

### 任务 03: task-03-send-size-in-attach

- objective: Update the client attach path to read terminal size before sending `Attach`, include `width,height`, and remove the immediate successful-attach `Resize`.
- files_or_artifacts:
  - crates/qscreen-client/src/main.rs
  - crates/qscreen-client/src/term.rs
- ordered_steps:
  - Find the attach command flow and terminal-size helper.
  - Read terminal size before constructing the `Attach` request.
  - Include the measured `width,height` in the initial `Attach`.
  - Remove the immediate post-attach `Resize` sent only to communicate initial size.
  - Keep later runtime resize handling unchanged.
- verification:
  - Run `cargo test -p qscreen-client`.
  - Review the attach path and confirm there is no immediate successful-attach `Resize`.
- done_definition: Client sends initial terminal size in `Attach`, does not send the redundant immediate resize, and still compiles/tests.
- rollback_or_retry_note: If terminal-size retrieval can fail, retry using the existing client fallback/error pattern instead of sending an attach without size.

### 任务 04: task-04-run-phase-checks

- objective: Run phase-level verification and inspect changed protocol/client behavior for dependency readiness.
- files_or_artifacts:
  - crates/qscreen-protocol/src
  - crates/qscreen-client/src/main.rs
  - crates/qscreen-client/src/term.rs
- ordered_steps:
  - Run protocol tests after protocol changes.
  - Run client tests after attach path changes.
  - Inspect JSON command shape for `Focus`.
  - Inspect attach request construction and validation tests for required size behavior.
  - Inspect client attach path to confirm the immediate post-attach `Resize` was removed while later resize events remain.
- verification:
  - `cargo test -p qscreen-protocol`
  - `cargo test -p qscreen-client`
  - Review checks listed in ordered steps pass.
- done_definition: All Phase 01 deliverables are implemented, verified, and ready for daemon phases to depend on the new attach size contract.
- rollback_or_retry_note: If checks fail, fix the smallest failing protocol or client task first; if the size contract must be reverted, revert protocol validation and client request-shape changes together.

## 自审

- deliverables_covered: yes
- dependency_order_valid: yes
- blockers_or_assumptions: none
- ready_for_execution: yes

## Override 记录

- none
