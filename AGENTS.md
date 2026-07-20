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

## Release Process

Formal releases use tags named `YYYYMMDD-NN`, where `NN` is the 2-digit release count for that date. Before tagging, fetch remote tags and choose the next unused number for the current date, for example `20260525-01`.

Release `qscn` as prebuilt binaries for:

- Linux x86_64: `x86_64-unknown-linux-gnu`
- Linux arm64: `aarch64-unknown-linux-gnu`
- macOS arm64: `aarch64-apple-darwin`
- Windows x86_64: `x86_64-pc-windows-gnu`
- Windows arm64: `aarch64-pc-windows-gnullvm`

Build optimized binaries with `--locked --bin qscn`. On macOS hosts, use `cargo zigbuild` for Linux and Windows cross builds when the native linker tools are unavailable:

- `cargo build --release --locked --bin qscn --target aarch64-apple-darwin`
- `cargo zigbuild --release --locked --bin qscn --target x86_64-unknown-linux-gnu`
- `cargo zigbuild --release --locked --bin qscn --target aarch64-unknown-linux-gnu`
- `cargo zigbuild --release --locked --bin qscn --target x86_64-pc-windows-gnu`
- `cargo zigbuild --release --locked --bin qscn --target aarch64-pc-windows-gnullvm`

Package each archive with a single executable at the archive root: `qscn` for Unix archives and `qscn.exe` for Windows archives. Use these asset names:

- `qscreen-<tag>-linux-x86_64.tar.gz`
- `qscreen-<tag>-linux-arm64.tar.gz`
- `qscreen-<tag>-macos-arm64.tar.gz`
- `qscreen-<tag>-windows-x86_64.zip`
- `qscreen-<tag>-windows-arm64.zip`
- `SHA256SUMS`

Additionally run `scripts/package-windows-gz.sh <tag>` to produce the per-asset Windows gz artifacts consumed by quicktui-installer's automatic qscn install:

- `qscn-windows-amd64.exe.gz` + `qscn-windows-amd64.exe.gz.sha256` + `qscn-windows-amd64.exe.sha256`
- `qscn-windows-arm64.exe.gz` + `qscn-windows-arm64.exe.gz.sha256` + `qscn-windows-arm64.exe.sha256`

These names carry no tag (release download URLs are tag-scoped) and use amd64/arm64 to match QuickTUI asset naming. Include them in `SHA256SUMS` as well.

Create the GitHub Release as a formal latest release, not a draft or prerelease:

- `git tag -a <tag> -m "Release <tag>"`
- `git push origin <tag>`
- `gh release create <tag> --title "<tag>" --generate-notes --latest`
- `gh release upload <tag> dist/<tag>/qscreen-<tag>-* dist/<tag>/qscn-windows-* dist/<tag>/SHA256SUMS`

Verify the release before handing off: `shasum -a 256 -c SHA256SUMS` must pass locally, `gh release view <tag> --json assets,isDraft,isPrerelease,tagName,url` must show all twelve assets uploaded (five archives, six `qscn-windows-*` files, `SHA256SUMS`), and the latest release should point at the new tag.

## Coding Style & Naming Conventions

Use Rust 2024 edition idioms and `rustfmt` defaults. Keep module names, file names, and functions in `snake_case`; types and enum variants use `PascalCase`; constants use `SCREAMING_SNAKE_CASE`. Prefer workspace dependencies in the root `Cargo.toml` for shared crates. Keep protocol JSON field names and compatibility behavior stable, especially `payload_b64` and size limits. Write all code comments in English.

## Testing Guidelines

Add focused unit tests near the code they cover. Use descriptive test names such as `round_trip_payload` or `validate_session_name_err`. Protocol and shared helper changes should include serialization, validation, and edge-case tests. For daemon/client changes, keep platform-specific behavior behind `#[cfg(windows)]` and make non-Windows tests avoid requiring a live ConPTY daemon.

## Change Workflow

All code changes must follow this mandatory review loop:

- Do every code change in a dedicated git worktree; never edit files directly on the main working tree.
- Create worktrees under `<main-tree>/worktrees/` (the `worktrees/` directory at the repository root).
- When the work in the worktree is complete, dispatch `codex gpt-5.6-high` to review the change.
- Every valid blocker/high/medium issue in the review must be fixed. Lower-severity or invalid findings may be noted but do not block the loop.
- After fixing, run `codex gpt-5.6-high` again and repeat this review-fix cycle until no blocker/high/medium issues remain.
- Once the review loop passes cleanly, merge the worktree branch into `main`, push `main` to the remote, and remove the worktree.

## Commit & Pull Request Guidelines

Use Conventional Commit subjects in the imperative mood, for example `feat(attach): pick highest-ID session by default` or `fix(daemon): handle shutdown race`. Pull requests should summarize the behavior change, list the test commands run, link any related issues, and include terminal output when CLI behavior changes. Note Windows-specific manual testing when touching daemon, pipe, or PTY code.

## Security & Configuration Tips

Do not commit local logs, build artifacts, or secrets. Daemon logs are written under `%TEMP%` as `qscreen-daemon-<user>.log`. Session names are limited to `[A-Za-z0-9._-]`; preserve that validation when adding new commands.
