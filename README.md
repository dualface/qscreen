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

- Windows uses named pipes and starts `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe` by default. Use `qscn new --shell cmd --session work` to start `C:\Windows\System32\cmd.exe` for a single session, or set the daemon environment variable `QSCREEN_WINDOWS_SHELL=cmd` or `QSCREEN_WINDOWS_SHELL=cmd.exe` to make cmd the daemon default. Explicit `powershell` and `powershell.exe` values keep the default PowerShell behavior. Unsupported values return an error and prevent session creation.
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
qscn new                     # create a timestamp-named session
qscn new --session work      # create and attach using an option name
qscn new --shell cmd         # create a timestamp-named cmd session on Windows
qscn new --shell cmd --session work
qscn attach work             # reattach to a session
qscn -r work                 # alias for attach
qscn ls                      # list sessions
qscn kill work               # terminate a session
qscn shutdown                # stop daemon and close sessions
```

Custom prefix keys:

```sh
qscn --prefix C-b attach work # attach with Ctrl+B as the session prefix
qscn --prefix C-b new work    # create and attach with Ctrl+B as the session prefix
qscn --prefix C-b             # smart launch with Ctrl+B as the session prefix
```

Prefix values accept `C-a` through `C-z` or `Ctrl+A` through `Ctrl+Z`.
`QSCREEN_PREFIX` sets a fallback prefix for every command:

```sh
QSCREEN_PREFIX=C-b qscn attach work
```

When both are set, `--prefix` takes precedence over `QSCREEN_PREFIX`.
When neither is set, `qscn` uses `Ctrl+A`.

Inside a session:

- `<prefix> d`: detach, leaving the session running.
- `<prefix> <prefix>`: send a literal prefix key to the shell.
- `<prefix> s`: open the session list; choose a detached session to switch attaches.

With the default prefix, those controls are `Ctrl+A d`, `Ctrl+A Ctrl+A`, and
`Ctrl+A s`. With `qscn --prefix C-b ...`, they are `Ctrl+B d`,
`Ctrl+B Ctrl+B`, and `Ctrl+B s`.

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
