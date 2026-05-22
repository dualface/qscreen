# Manual Validation: Multi-Attach Clients

## Context

- workflow_id: wf-multi-attach-clients-20260522-36f5
- phase_id: phase-04-tests-docs-validation
- recorded_at: 2026-05-22T08:56:26Z
- host: Darwin colorize.local 25.5.0 arm64
- rustc: 1.95.0, host aarch64-apple-darwin
- Windows daemon runtime: unavailable on this host

## Automated Host Validation

Automated host validation was run on macOS without a live Windows ConPTY daemon:

- `cargo test --workspace`: passed, including protocol focus command, client list attached formatting, session multi-client attach/detach, broadcast, close notification, focus/input sizing, handler second attach, handler detach, handler focus, and handler kill notification tests.

These tests cover deterministic protocol, formatting, session, and in-memory handler behavior. They do not prove behavior through Windows named pipes or the Windows ConPTY runtime.

## Windows Runtime Checklist

Status: not performed. The current host is macOS (`aarch64-apple-darwin`), while daemon runtime validation for production behavior must be performed on Windows because named-pipe and ConPTY behavior are Windows-specific for the supported daemon target.

Required retry environment:

- Windows host with Rust stable and `x86_64-pc-windows-gnu` target installed.
- A terminal capable of focus reporting and resize events.
- Built `qscn.exe` from this workspace.

### Multi-Terminal Attach

Not performed on this host.

Retry checklist on Windows:

1. Start a session: `qscn new work`.
2. Attach a second terminal: `qscn attach work`.
3. Produce output inside the session and confirm both terminals receive it.
4. Detach one terminal with `Ctrl+A D` and confirm the other stays attached and interactive.
5. Run `qscn ls` and confirm the session remains `attached` while at least one client is attached.
6. Detach all clients and confirm `qscn ls` reports `detached`.
7. Kill the session with `qscn kill work` and confirm all attached clients receive an exit notification.

Blocker: Windows runtime unavailable on current macOS host; no named-pipe or ConPTY process was started.

### Size Ownership

Not performed on this host.

Retry checklist on Windows:

1. Attach client A at one terminal size.
2. Attach client B at a different terminal size and confirm the PTY follows B after attach.
3. Resize inactive client A and confirm the PTY does not immediately follow A.
4. Focus client A and confirm the PTY applies A's stored size.
5. Resize inactive client B and confirm the PTY does not immediately follow B.
6. Send input from client B and confirm B becomes active and the PTY applies B's stored size.
7. Resize the active client and confirm the PTY resizes immediately.

Blocker: Windows runtime unavailable on current macOS host; focus/input/resize behavior was validated only through deterministic unit tests.
