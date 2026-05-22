# qscreen

`qscreen` is a lightweight terminal session manager. It keeps shell sessions alive in a background daemon, lets you detach and reattach, and provides a small `tmux`-style command set through the `qscn` executable.

## Features

- Create, list, attach, detach, and kill terminal sessions.
- Smart default command: create `main`, attach the only session, or list multiple sessions.
- Background daemon starts on demand.
- Scrollback replay when reattaching.
- Terminal resize forwarding.
- Windows, Linux, and macOS support.

## Platform Notes

- Windows uses named pipes and starts Windows PowerShell.
- Linux/macOS use Unix domain sockets and start `$SHELL -l`, falling back to `/bin/sh -l`.
- Session names must match `[A-Za-z0-9._-]` and be at most 64 characters.

## Build

Requires stable Rust. The workspace is Rust 2024 edition.

```sh
cargo build
cargo build --release
cargo test
```

The Makefile wraps common Cargo commands:

```sh
make build
make release
make test
make clean
```

## Usage

```sh
qscn                         # smart launch
qscn new work                # create and attach to a session
qscn new                     # create a timestamp-named session
qscn attach work             # reattach to a session
qscn -r work                 # alias for attach
qscn ls                      # list sessions
qscn kill work               # terminate a session
qscn shutdown                # stop daemon and close sessions
```

Inside a session:

- `Ctrl+A D`: detach, leaving the session running.
- `Ctrl+A Ctrl+A`: send a literal `Ctrl+A` to the shell.

`qscn ls` prints:

```text
<name>  <state>  <created-at>  <terminal-size>
```

States are `attached`, `detached`, or `exited(<code>)`.

## Project Layout

- `crates/qscreen-client`: CLI binary, terminal UI, daemon launcher.
- `crates/qscreen-daemon`: session state, PTY lifecycle, IPC server.
- `crates/qscreen-protocol`: JSON-line wire protocol and validation.
- `crates/qscreen-shared`: shared IPC names, paths, and user helpers.

## Development

Before sending changes:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

For Windows cross-checks:

```sh
cargo check --workspace --target x86_64-pc-windows-gnu
```
