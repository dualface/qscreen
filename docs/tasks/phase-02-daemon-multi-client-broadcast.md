# 阶段任务清单: phase-02-daemon-multi-client-broadcast

## 状态

- workflow_id: wf-multi-attach-clients-20260522-36f5
- phase_id: phase-02-daemon-multi-client-broadcast
- phase_plan_path: /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/docs/plans/wf-multi-attach-clients-20260522-36f5.md
- self_review_verdict: pass
- owner: codex-worker

## 阶段目标

Replace the daemon's single attached-client slot with per-client state so multiple clients can attach to one session, receive scrollback and broadcast PTY output, and detach or disconnect without disrupting other attached clients.

## 任务

### 任务 01: task-01-model-attached-clients

- objective: Introduce the session data model for multiple attached clients and active-client bookkeeping.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src/session.rs
- ordered_steps:
  - Define or update the client id type used by session attach/detach methods.
  - Replace the single attached-client field with a monotonically increasing next client id, `HashMap<ClientId, AttachedClient>`, and optional active client id.
  - Ensure `AttachedClient` stores the sender plus the client's latest terminal size.
  - Update `is_attached()` so it returns true when the attached-client map is non-empty.
  - Adjust internal callers and tests that referenced the old single-client field.
- verification:
  - `cargo test -p qscreen-daemon`
  - Review `session.rs` to confirm there is no remaining single attached-client slot used as session state.
- done_definition: Session state compiles with multi-client storage, active-client tracking, and count-based `is_attached()` semantics.
- rollback_or_retry_note: Revert the session struct and any direct callers to the pre-phase single-attacher fields if the model change blocks compilation before behavior is implemented.

### 任务 02: task-02-implement-session-multiclient

- objective: Implement attach, broadcast, detach, exit, and kill behavior on top of the multi-client session model.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src/session.rs
- ordered_steps:
  - Update `attach` to allocate a new client id, store sender and attach size, mark the new client active, resize the PTY to the attach size, and return both the client id and scrollback needed by the handler.
  - Change PTY output handling so each output chunk is sent to every attached client.
  - Remove only clients whose broadcast sender fails, leaving other clients attached.
  - Update `detach(client_id)` so it removes only the specified client and clears the active client id only when that client was active.
  - Update session exit and kill paths to notify all attached clients and clear the attached-client map.
- verification:
  - `cargo test -p qscreen-daemon`
  - Session-level tests or review show two clients can attach, both receive PTY output, and detaching one leaves the other attached.
  - Broadcast failure test or review shows only failed senders are removed.
- done_definition: Session behavior satisfies all Phase 02 daemon-state deliverables without depending on handler connection cleanup.
- rollback_or_retry_note: If broadcast or cleanup semantics regress, retry by isolating behavior behind small session helper methods before changing handler code.

### 任务 03: task-03-wire-handler-client-id

- objective: Track each attach connection's client id in the daemon handler and detach the correct client on every cleanup path.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src/lib.rs
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src/session.rs
- ordered_steps:
  - Update the attach handler to capture the `client_id` returned by session `attach`.
  - Ensure explicit detach from that connection calls `detach(client_id)`.
  - Ensure disconnect, failed initial response, failed scrollback write, and writer shutdown detach only that connection's client id.
  - Keep Phase 01 attach-size validation intact before session registration.
  - Preserve existing single-client behavior for one attached client while allowing additional clients to attach.
- verification:
  - `cargo test -p qscreen-daemon`
  - Review handler cleanup branches to confirm every post-attach failure path has access to the same connection-local `client_id`.
- done_definition: Daemon handler no longer performs global detach for one connection and all known attach cleanup paths remove only the connection's own client.
- rollback_or_retry_note: If handler control flow becomes unclear, retry by introducing a narrow connection guard or helper that owns optional `client_id` cleanup.

### 任务 04: task-04-cover-daemon-multiclient

- objective: Add focused daemon tests for Phase 02 multi-client behavior and run required daemon verification.
- files_or_artifacts:
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src/session.rs
  - /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/crates/qscreen-daemon/src/lib.rs
- ordered_steps:
  - Add or update session tests proving multiple clients can register and detaching one preserves the other.
  - Add or update a broadcast failure test proving only failed clients are removed.
  - Add or update coverage for session exit or kill notifying all attached clients when practical in the current test harness.
  - Run daemon tests and fix failures caused by the Phase 02 changes.
  - Record any handler-level coverage that is not practical until Phase 04 as an explicit review note in the implementation handoff.
- verification:
  - `cargo test -p qscreen-daemon`
  - Existing single-client attach tests still pass.
  - New tests cover independent detach and failed broadcast cleanup.
- done_definition: Phase 02 has focused daemon coverage for multi-client attach, detach, and failed broadcast cleanup, with any deferred handler coverage explicitly documented for Phase 04.
- rollback_or_retry_note: If tests require platform-only PTY behavior, keep unit tests on pure session logic and defer live handler coverage to Phase 04 validation notes.

## 自审

- deliverables_covered: yes
- dependency_order_valid: yes
- blockers_or_assumptions: Assumes phase-01-protocol-client-handshake has already provided required attach width/height validation and daemon attach-size enforcement.
- ready_for_execution: yes

## Override 记录

- none
