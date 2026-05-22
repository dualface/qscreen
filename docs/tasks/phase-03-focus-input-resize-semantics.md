# 阶段任务清单: phase-03-focus-input-resize-semantics

## 状态

- workflow_id: wf-multi-attach-clients-20260522-36f5
- phase_id: phase-03-focus-input-resize-semantics
- phase_plan_path: /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/docs/plans/wf-multi-attach-clients-20260522-36f5.md
- self_review_verdict: pass
- owner: codex-worker

## 阶段目标

Implement latest-client sizing semantics across client focus/input/resize events and daemon per-client state, building on the multi-client model from phase-02.

## 任务

### 任务 01: task-01-enable-focus-reporting

- objective: Enable terminal focus reporting during attach UI lifetime and disable it during all terminal cleanup paths.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-client/src/term.rs
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-client/src/main.rs
- ordered_steps:
  - Locate the raw-mode setup and cleanup/drop paths used by the attach UI.
  - Emit `ESC[?1004h` after raw mode starts and before the attach event loop begins.
  - Emit `ESC[?1004l` in cleanup together with existing terminal restoration.
  - Ensure cleanup is used for normal detach, attach error, PTY exit, client disconnect, and process-exit paths already covered by the attach UI.
- verification:
  - `cargo test -p qscreen-client`
  - Review confirms enable runs after raw mode setup and disable is in the shared cleanup path, not only the happy path.
- done_definition: Focus reporting is enabled only for active attach UI lifetime and disabled by terminal cleanup for every existing exit path.
- rollback_or_retry_note: If cleanup integration is unsafe, revert the focus-reporting escape writes only and keep existing raw-mode cleanup unchanged.

### 任务 02: task-02-map-focus-events

- objective: Convert terminal focus-gained events into async attach actions and intentionally ignore focus-lost events.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-client/src/main.rs
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-client/src/term.rs
- ordered_steps:
  - Locate the blocking event reader and its attach-loop action channel.
  - Add or reuse an attach action variant for focus gained.
  - Map `FocusGained` to that action.
  - Ignore `FocusLost` without sending any command or changing local state.
  - Keep existing input, resize, detach, and error event behavior unchanged.
- verification:
  - `cargo test -p qscreen-client`
  - Tests or code review confirm `FocusLost` has no outbound action.
- done_definition: `FocusGained` reaches the async attach loop as a distinct action and `FocusLost` is a no-op.
- rollback_or_retry_note: If focus events are not available under the current event API, gate the mapping behind existing event support and document the limitation in the task result.

### 任务 03: task-03-send-focus-command

- objective: Send protocol `Command::Focus` from the async attach loop when a focus action is received.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-client/src/main.rs
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-protocol/src
- ordered_steps:
  - Locate where async attach actions are converted into protocol commands.
  - Add handling for the focus action that writes `Command::Focus` to the daemon connection.
  - Preserve existing error handling and detach behavior for failed command writes.
  - Confirm the protocol command already exists from phase-01 before wiring the client path.
- verification:
  - `cargo test -p qscreen-client`
  - `cargo test -p qscreen-protocol`
  - Review confirms focus action sends `Command::Focus` and does not send input or resize payloads.
- done_definition: Client focus-gained events produce one `Command::Focus` on the attach connection.
- rollback_or_retry_note: If protocol command shape differs from the phase plan, adapt the client send path to the existing serialized command instead of changing protocol scope in this phase.

### 任务 04: task-04-add-session-active-size-apis

- objective: Add daemon session operations for focus, input ownership, and per-client resize storage.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src/session.rs
- ordered_steps:
  - Locate the phase-02 per-client state, active client id, PTY resize, and PTY write helpers.
  - Add or adjust a focus operation that marks a client active and resizes PTY to that client's stored size.
  - Add or adjust an input operation that marks a client active, resizes PTY to that client's stored size, then writes payload bytes to PTY.
  - Add or adjust a resize operation that updates only the requesting client's stored size.
  - In resize handling, resize PTY immediately only when the requesting client is active.
  - Keep failed, detached, or unknown client behavior consistent with existing session error handling.
- verification:
  - `cargo test -p qscreen-daemon`
  - Tests or review prove input-triggered resize happens before PTY write.
  - Tests or review prove inactive resize stores size without immediate PTY resize.
- done_definition: Session-level APIs enforce latest-active-client PTY size ownership for focus, input, and resize.
- rollback_or_retry_note: If existing session helpers make one combined API clearer, keep behavior equivalent and avoid broad session lifecycle refactors.

### 任务 05: task-05-wire-daemon-attach-handler

- objective: Route attached-client `Focus`, `Input`, and `Resize` commands through the connection's `client_id` while keeping non-attach command paths compiling.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src/lib.rs
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src/session.rs
- ordered_steps:
  - Locate `handle_attach` command dispatch and the connection-local `client_id` introduced in phase-02.
  - Dispatch `Command::Focus` to the session focus operation with that `client_id`.
  - Dispatch attached-client input to the session input operation with that `client_id`.
  - Dispatch attached-client resize to the session resize operation with that `client_id`.
  - Leave ordinary non-attach command handling compiling, with normal attached-client semantics kept in `handle_attach`.
  - Preserve existing detach, disconnect, failed write, and cleanup paths.
- verification:
  - `cargo test -p qscreen-daemon`
  - Review confirms no global active-client guess is used; all attached command handling uses the connection's `client_id`.
- done_definition: Daemon command handling applies focus/input/resize semantics to the correct attached client and preserves non-attach command compilation.
- rollback_or_retry_note: If handler structure conflicts with new session APIs, adjust only the attach dispatch boundary and avoid changing unrelated daemon commands.

### 任务 06: task-06-run-phase-verification

- objective: Run phase verification and record any platform or environment limits for follow-up phase-04 validation.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-client/src
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-protocol/src
- ordered_steps:
  - Run client tests after focus reporting and event mapping changes.
  - Run daemon tests after session and handler changes.
  - Run protocol tests if client focus command wiring depends on protocol command serialization.
  - Review the final diff for the three required semantic proofs: focus-lost ignored, input resize before PTY write, inactive resize stores without immediate PTY resize.
  - Note any host limitation for Windows-only runtime behavior without blocking unit-testable work.
- verification:
  - `cargo test -p qscreen-client`
  - `cargo test -p qscreen-daemon`
  - `cargo test -p qscreen-protocol`
- done_definition: Required crate tests pass or failures are documented with concrete cause, and all phase-03 semantic checks are verified by tests or code review.
- rollback_or_retry_note: If a test failure is unrelated to phase-03 edits, document it and keep the phase implementation scoped; if related, retry from the smallest failing task.

## 自审

- deliverables_covered: yes
- dependency_order_valid: yes
- blockers_or_assumptions: phase-01 has already added `Command::Focus` and required attach size; phase-02 has already added per-client session state and connection-local `client_id`.
- ready_for_execution: yes

## Override 记录

- none
