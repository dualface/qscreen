# 阶段计划清单: wf-multi-attach-clients-20260522-36f5

## 状态

- workflow_id: wf-multi-attach-clients-20260522-36f5
- spec_path: /Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/docs/specs/wf-multi-attach-clients-20260522-36f5.md
- self_review_verdict: pass
- owner: codex-worker

## 依赖摘要

- Phase 01 establishes the wire protocol and client attach request shape needed by all daemon work.
- Phase 02 replaces daemon single-attacher state with multi-client bookkeeping and broadcast semantics.
- Phase 03 layers focused/latest-client sizing semantics on top of the multi-client client_id model.
- Phase 04 closes coverage with tests, docs, formatting, linting, host validation, and Windows target checks.

## 阶段

### 阶段 01: phase-01-protocol-client-handshake

- title: Protocol and client attach handshake
- goal: Add the `Focus` command and require `Attach` requests to carry valid terminal size, while changing the client to send its initial size in `Attach` and stop sending the immediate post-attach `Resize`.
- depends_on: none
- deliverables:
  - `Command::Focus` added to protocol command enum and JSON serialization/deserialization.
  - `Attach` validation rejects missing or invalid `width,height`.
  - Daemon attach handler enforces the required `Attach` size contract at runtime.
  - Client attach path reads terminal size before sending `Attach`.
  - Client sends `Attach` with `width,height` and removes the immediate successful-attach `Resize`.
  - Protocol tests cover `Focus` round trip and `Attach` size validation.
- likely_files_or_modules:
  - crates/qscreen-protocol/src
  - crates/qscreen-client/src/main.rs
  - crates/qscreen-client/src/term.rs
  - crates/qscreen-daemon/src/lib.rs
- verification:
  - `cargo test -p qscreen-protocol`
  - `cargo test -p qscreen-daemon`
  - Focus command round-trip test passes.
  - Attach without valid size is rejected by validation tests and daemon attach handling.
  - Client code path has no immediate post-attach resize after successful attach.
- risks_and_rollback:
  - Risk: changing `Attach` validation can break existing attach tests that assumed missing size was accepted.
  - Risk: daemon attach validation is a narrow handler change in Phase 01, before broader daemon session-model work in Phase 02.
  - Rollback: revert protocol validation and client request-shape changes together, because daemon phases depend on the new attach size contract.
  - Scope rationale: `Attach` size validation is not complete unless the daemon attach entry point calls the protocol helper; this phase may touch only the daemon handler import and pre-session attach validation path, not session lifecycle or multi-client state.
- self_review_verdict: pass

### 阶段 02: phase-02-daemon-multi-client-broadcast

- title: Daemon multi-client session model
- goal: Replace the single attached-client slot with per-client state so multiple clients can attach, receive scrollback, receive broadcast PTY output, and detach independently.
- depends_on: phase-01-protocol-client-handshake
- deliverables:
  - Session stores a monotonically increasing client id, `HashMap<ClientId, AttachedClient>`, and optional active client id.
  - `attach` allocates a client id, stores sender and size, marks the new client active, resizes PTY to attach size, and returns scrollback.
  - PTY output broadcasts to all attached clients.
  - Failed broadcast sends remove only the failed client.
  - `detach(client_id)` removes only that client and clears active owner if needed.
  - Session exit and kill notify all clients and clear the attached-client map.
  - `is_attached()` returns true when attached client count is greater than zero.
  - Attach handler tracks the connection's `client_id` and detaches it on explicit detach, disconnect, failed initial response, failed scrollback write, or writer shutdown.
- likely_files_or_modules:
  - crates/qscreen-daemon/src/session.rs
  - crates/qscreen-daemon/src/lib.rs
- verification:
  - `cargo test -p qscreen-daemon`
  - Session-level tests show multiple clients can register and detaching one preserves the other.
  - Broadcast failure test shows only failed clients are removed.
  - Existing single-client attach behavior remains covered.
- risks_and_rollback:
  - Risk: missed cleanup paths can leave stale clients or broken senders in session state.
  - Rollback: restore single-attacher state and handler paths if multi-client bookkeeping causes lifecycle regressions before Phase 03 starts.
- self_review_verdict: pass

### 阶段 03: phase-03-focus-input-resize-semantics

- title: Focus, input, and resize ownership
- goal: Implement latest-client sizing semantics across client events and daemon per-client state.
- depends_on: phase-02-daemon-multi-client-broadcast
- deliverables:
  - Client enables focus reporting after raw mode starts with `ESC[?1004h`.
  - Client disables focus reporting during cleanup with `ESC[?1004l`.
  - Blocking event reader maps `FocusGained` into an async attach action and ignores `FocusLost`.
  - Async attach loop sends `Command::Focus` for focus actions.
  - Daemon handles `Focus` for the connection's `client_id`, marks that client active, and resizes PTY to its stored size.
  - Daemon handles input from a client by marking it active, resizing PTY to its stored size, then writing payload bytes.
  - Daemon handles resize by updating only that client's stored size and resizing immediately only when that client is active.
  - Non-attach command paths remain compiling while normal attached-client semantics live in `handle_attach`.
- likely_files_or_modules:
  - crates/qscreen-client/src/main.rs
  - crates/qscreen-client/src/term.rs
  - crates/qscreen-daemon/src/session.rs
  - crates/qscreen-daemon/src/lib.rs
- verification:
  - `cargo test -p qscreen-client`
  - `cargo test -p qscreen-daemon`
  - Tests or review prove focus-lost is ignored.
  - Tests or review prove input-triggered resize happens before PTY write.
  - Tests or review prove inactive resize stores size without immediate PTY resize.
- risks_and_rollback:
  - Risk: focus reporting cleanup must run on attach errors, detach, PTY exit, client disconnect, and process exit without relying on cancellable event-reader threads.
  - Rollback: keep daemon multi-client broadcast from Phase 02 and disable focus reporting plus focus command handling if terminal cleanup issues appear.
- self_review_verdict: pass

### 阶段 04: phase-04-tests-docs-validation

- title: Tests, docs, and cross-platform validation
- goal: Complete required automated coverage, documentation updates, and build validation for host and Windows target.
- depends_on:
  - phase-01-protocol-client-handshake
  - phase-02-daemon-multi-client-broadcast
  - phase-03-focus-input-resize-semantics
- deliverables:
  - README and README_CN document multiple attach behavior and focus/input based latest-client sizing at a high level.
  - Tests cover `qscn ls` boolean attached semantics where practical.
  - Tests cover session exit or kill notifying all attached clients where practical.
  - Daemon handler tests cover second attach, explicit detach, and focus command if practical with in-memory streams.
  - Manual validation notes for multi-terminal attach and size ownership are recorded if performed.
  - Formatting, unit tests, clippy, and Windows target check are run or blockers documented.
- likely_files_or_modules:
  - README.md
  - README_CN.md
  - crates/qscreen-protocol/src
  - crates/qscreen-client/src
  - crates/qscreen-daemon/src
  - crates/qscreen-shared/src
- verification:
  - `cargo fmt --all`
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets`
  - `cargo check --workspace --target x86_64-pc-windows-gnu`
  - Manual multi-terminal validation follows the spec checklist when Windows daemon runtime is available.
  - Manual size validation follows the spec checklist when Windows daemon runtime is available.
- risks_and_rollback:
  - Risk: Windows target or manual runtime validation may expose platform-specific IPC or ConPTY issues not visible in host tests.
  - Rollback: keep docs and tests aligned to implemented behavior, and revert the narrow failing phase rather than documentation-only changes.
- self_review_verdict: pass

## 自审

- coverage_against_spec: pass. The phases cover protocol command and validation, client attach request changes, daemon multi-client bookkeeping and broadcast, per-client detach and broken-writer cleanup, focus/input/resize active-size semantics, docs, automated tests, and required validation commands.
- missing_items: none
- handoff_ready_for_tasking: yes

## Override 记录

- none
