# Spec: Multiple Attached Clients per Session

Workflow ID: `wf-multi-attach-clients-20260522-36f5`

## Goal

Allow multiple `qscn` clients to attach to the same live session at the same time.

All attached clients should receive the same PTY output stream. Any attached client may send input to the session. Terminal sizing must follow a focused/latest-client strategy:

- `Attach` includes the attaching client's current screen size.
- A focus-gained event from a client makes that client the active size owner and resizes the PTY to that client's last known size.
- An input event from a client also makes that client the active size owner, resizes the PTY to that client's last known size, then writes the input.
- A resize event updates that client's stored size. If that client is currently active, resize the PTY immediately.
- Focus-lost events are ignored.

This should remove the current `session "name" is already attached` behavior for live sessions.

## Non-Goals

- Do not implement collaborative cursor indicators, per-client identity UI, permissions, read-only attach, or input locking.
- Do not implement tmux's full window-size policy matrix.
- Do not preserve backward compatibility with older qscreen clients that send `Attach` without `width,height`.
- Do not change session-name validation or daemon startup behavior.
- Do not redesign the terminal renderer beyond what is needed for multi-attach correctness.
- Do not add new external runtime services or a TUI framework.

## Constraints

- Rust 2024 edition and `rustfmt` defaults.
- Preserve JSON-line protocol style and existing field names, especially `payload_b64`.
- Keep `payload_b64` size limits and protocol validation intact.
- Keep Windows, Linux, and macOS builds compiling.
- Keep daemon runtime support guarded by existing platform-specific IPC code.
- Daemon session lifecycle must remain safe when one of multiple clients disconnects, detaches, or has a broken writer.
- Raw mode and terminal cleanup must remain robust on attach errors, detach, PTY exit, client disconnect, and process exit.
- The existing blocking `crossterm::event::read()` worker is hard to cancel; implementation must not depend on graceful cancellation of that thread.

## User-Facing Behavior

### Multiple Attach

- If session `work` exists and one client is attached, a second `qscn attach work` succeeds.
- Each attaching client receives the session scrollback, then live PTY output.
- PTY output is broadcast to all currently attached clients.
- If one client detaches with `Ctrl+A D`, only that client exits attach mode. Other attached clients remain attached and continue receiving output.
- If one client process dies or its IPC writer fails, only that client is removed from the attached-client set.
- If the session exits or is killed, all attached clients receive an exit event and leave attach mode.
- `qscn ls` may continue to show `attached` as a boolean state meaning attached count is greater than zero.

### Input

- Any attached client may send input.
- Daemon processes input in the order received by the session's daemon-side command loop.
- Before writing input from client A, daemon resizes the PTY to client A's last known size and marks client A active.
- Concurrent inputs from multiple clients do not need additional arbitration beyond daemon receive order.

### Resize

- `Attach` request carries `width,height`; these values are required and validated.
- Client no longer sends a separate immediate `Resize` after successful attach.
- `Resize` from client A updates only client A's stored size.
- If client A is active when the resize arrives, daemon immediately resizes the PTY to client A's new size.
- If client A is not active, daemon stores the size but does not resize the PTY until client A focuses or sends input.

### Focus

- Client enables terminal focus reporting while attached.
- Client handles focus-gained events and sends a daemon `Focus` command.
- Client ignores focus-lost events.
- Daemon handles `Focus` by marking that client active and resizing the PTY to that client's stored size.
- If a terminal does not support focus reporting, behavior degrades to the input-triggered resize fallback.

## Architecture and Boundaries

### Protocol

Add a new command:

```rust
pub enum Command {
    New,
    List,
    Attach,
    Detach,
    Input,
    Resize,
    Focus,
    Kill,
    Stop,
}
```

Protocol behavior:

- `Attach` requires valid `width,height`.
- `Resize` keeps existing `width,height` validation.
- `Focus` requires a valid session name and no payload.
- Protocol serialization/deserialization tests must cover `Focus`.
- Existing `SessionInfo.attached: bool` remains valid and means at least one attached client exists.

No new wire fields are required because `Message` already has `width,height`.

### Client

Primary files:

- `crates/qscreen-client/src/main.rs`
- `crates/qscreen-client/src/term.rs` only if rendering assumptions require small adjustments

Client changes:

- In `attach_session`, get terminal size before sending `Attach`.
- Send `Attach { width, height }`.
- Remove the immediate post-attach `Resize` request.
- Enable focus reporting after raw mode starts by writing `ESC[?1004h`.
- Disable focus reporting during cleanup by writing `ESC[?1004l`.
- In the blocking event reader:
  - keep current key handling and detach prefix behavior;
  - on resize, send a resize action to the async loop;
  - on `crossterm::event::Event::FocusGained`, send a focus action to the async loop;
  - ignore `FocusLost`.
- In the async attach loop, translate focus action into a `Command::Focus` request.

### Daemon

Primary files:

- `crates/qscreen-daemon/src/session.rs`
- `crates/qscreen-daemon/src/lib.rs`

Replace the single attached client state:

```rust
Option<mpsc::UnboundedSender<SessionEvent>>
```

with a multi-client structure similar to:

```rust
struct AttachedClient {
    tx: mpsc::UnboundedSender<SessionEvent>,
    width: u16,
    height: u16,
}
```

Session should also track:

- monotonically increasing client id;
- `HashMap<ClientId, AttachedClient>`;
- current active client id, if any.

Suggested session methods:

```rust
pub fn attach(
    &self,
    tx: mpsc::UnboundedSender<SessionEvent>,
    width: u32,
    height: u32,
) -> anyhow::Result<(ClientId, Vec<u8>)>;

pub fn detach(&self, client_id: ClientId);
pub fn update_client_size(&self, client_id: ClientId, width: u32, height: u32) -> anyhow::Result<()>;
pub fn focus_client(&self, client_id: ClientId) -> anyhow::Result<()>;
pub fn write_input_from_client(&self, client_id: ClientId, data: &[u8]) -> anyhow::Result<()>;
pub fn is_attached(&self) -> bool;
```

Behavior details:

- `attach` validates non-exited and non-closed session state, allocates a client id, stores sender and size, marks that client active, resizes PTY to the attach size, then returns scrollback.
- PTY reader broadcasts each output chunk to all attached clients.
- Broadcast removes clients whose channel send fails.
- session exit and kill broadcast exit to all clients and clear the attached-client map.
- `detach(client_id)` removes only that client. If detached client was active, active client id may become `None`; daemon does not need to resize to another client until focus/input/active resize arrives.
- attach handler must call `detach(client_id)` on client disconnect, explicit detach, failed initial response, failed scrollback write, or writer-task shutdown.
- `Input`, `Resize`, `Focus`, and `Detach` inside an attach loop must operate on that connection's `client_id`.
- Non-attach command dispatch can keep existing `Input` and `Resize` support for daemon control paths, but normal attached-client semantics live in `handle_attach`.

### Documentation

Update README and README_CN:

- mention that multiple clients can attach to the same session;
- document focus/input based latest-client sizing at a high level;
- keep examples aligned with `qscn`.

## Success Criteria

- Two or more clients can attach to the same live session without `already attached` error.
- All attached clients receive live PTY output.
- Scrollback replay still works for every attaching client.
- Detaching one client leaves other clients attached.
- Dead/broken client writers are removed without breaking other attached clients.
- Session exit and kill notify all attached clients.
- `qscn ls` shows `attached` when at least one client is attached.
- `Attach` without valid `width,height` is rejected.
- Client sends attach size in the `Attach` request and no longer sends immediate post-attach resize.
- Focus-gained from client A resizes PTY to client A's stored size.
- Focus-lost is ignored.
- Input from client A resizes PTY to client A's stored size before writing input.
- Resize from active client immediately resizes PTY; resize from inactive client only updates stored size.
- Existing single-client attach behavior still works.
- Existing tests pass on host.
- Windows target check passes.

## Test and Validation Expectations

Automated tests:

- Protocol test for `Command::Focus` round trip.
- Protocol or daemon tests that `Attach` rejects missing/invalid size.
- Session-level tests for attached-client bookkeeping where possible without a live PTY:
  - multiple clients can be registered;
  - detaching one client preserves the other;
  - `is_attached` reflects attached count greater than zero;
  - failed broadcast removes only failed clients.
- Daemon handler tests, if practical with in-memory duplex streams:
  - second attach to same session succeeds;
  - explicit detach removes only that client;
  - focus command returns ok and uses the connection's client id.
- Existing protocol/shared/client tests remain green.

Manual or integration validation:

- `cargo fmt --all`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets`
- `cargo check --workspace --target x86_64-pc-windows-gnu`
- Manual multi-terminal validation:
  - create session `work`;
  - attach from terminal A;
  - attach from terminal B;
  - run commands from A and verify B receives output;
  - run commands from B and verify A receives output;
  - detach A and verify B remains live;
  - kill or exit the session and verify all attached clients exit attach mode.
- Manual size validation:
  - attach clients with visibly different terminal sizes;
  - focus A and verify PTY reports A size, for example through `stty size` or equivalent shell command;
  - focus B and verify PTY reports B size;
  - resize inactive client and verify PTY size does not change until that client focuses or sends input;
  - type in inactive client and verify input-triggered resize applies before command execution.

## Phase Assumptions

1. Phase 1: Protocol and client attach handshake change.
2. Phase 2: Daemon session multi-client model and output broadcasting.
3. Phase 3: Client focus reporting and daemon focus/input/resize active-size semantics.
4. Phase 4: Tests, docs, and cross-platform validation.

## Open Questions

- Should `qscn ls` later expose attached client count? This spec keeps the existing boolean only.
- Should a newly attached client immediately become active and resize the PTY? This spec says yes.

## External Dependency Risks

- Terminal focus reporting: some terminals, SSH paths, nested terminal environments, or Windows terminal combinations may not emit focus-gained events consistently. Mitigation: input-triggered resize remains mandatory fallback, and focus-lost is ignored.
- `crossterm` focus event support: implementation depends on `crossterm` exposing focus-gained events for the target platforms. Mitigation: verify against the pinned `crossterm = 0.28` API during implementation; if unsupported on some platform, keep compile guards or parse raw focus sequence only where needed.

## Review History

none

## Approval Status

pending

## Override Record

none
