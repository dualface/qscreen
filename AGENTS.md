# Repository Guidelines

## Project Structure & Module Organization

This is a Rust workspace for `qscreen`, a lightweight terminal session manager. Workspace members live under `crates/`:

- `crates/qscreen-client`: `qscn` binary, CLI parsing, terminal attach UI, daemon launch mode.
- `crates/qscreen-daemon`: daemon state, session lifecycle, Windows named-pipe handling, PTY integration.
- `crates/qscreen-protocol`: JSON-line wire protocol, message types, validation helpers, compatibility constants.
- `crates/qscreen-shared`: shared paths and pipe-name helpers.

Tests are currently inline in crate source files with `#[cfg(test)] mod tests`. Build output stays in `target/`.

## Build, Test, and Development Commands

- `make build` or `cargo build`: compile the full workspace.
- `make release` or `cargo build --release`: create optimized binaries.
- `make test` or `cargo test`: run all unit tests.
- `cargo test -p qscreen-protocol`: run tests for one crate.
- `cargo fmt --all`: format all Rust code.
- `cargo clippy --workspace --all-targets`: run lint checks before larger changes.
- `make clean`: remove Cargo build artifacts.

The pinned toolchain is stable Rust with the `x86_64-pc-windows-gnu` target in `rust-toolchain.toml`. Daemon runtime support is Windows-only because it uses ConPTY and Windows named pipes.

## Coding Style & Naming Conventions

Use Rust 2024 edition idioms and `rustfmt` defaults. Keep module names, file names, and functions in `snake_case`; types and enum variants use `PascalCase`; constants use `SCREAMING_SNAKE_CASE`. Prefer workspace dependencies in the root `Cargo.toml` for shared crates. Keep protocol JSON field names and compatibility behavior stable, especially `payload_b64` and size limits.

## Testing Guidelines

Add focused unit tests near the code they cover. Use descriptive test names such as `round_trip_payload` or `validate_session_name_err`. Protocol and shared helper changes should include serialization, validation, and edge-case tests. For daemon/client changes, keep platform-specific behavior behind `#[cfg(windows)]` and make non-Windows tests avoid requiring a live ConPTY daemon.

## Commit & Pull Request Guidelines

Git history currently has only `Init import`, so no strict convention is established. Use concise imperative commit subjects, for example `Add resize validation tests` or `Fix daemon shutdown handling`. Pull requests should include a short behavior summary, test commands run, linked issues if any, and screenshots or terminal output when CLI behavior changes. Note Windows-specific manual testing when touching daemon, pipe, or PTY code.

## Security & Configuration Tips

Do not commit local logs, build artifacts, or secrets. Daemon logs are written under `%TEMP%` as `qscreen-daemon-<user>.log`. Session names are limited to `[A-Za-z0-9._-]`; preserve that validation when adding new commands.
