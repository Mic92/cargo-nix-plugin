# Contributing to cargo-nix-plugin

Thank you for your interest in contributing. This document explains
the process and what to expect.

## Contributor License Agreement

Before we can accept your contribution, you must sign our Contributor
License Agreement. The CLA bot will prompt you on your first pull
request; sign by replying to the bot's comment. This is a one-time
requirement per contributor. The agreement text is in [CLA.md](CLA.md).

If you are contributing on behalf of your employer and your employer
may have rights in your contribution, your employer should sign our
Corporate CLA. Contact opensource-cla@anthropic.com to arrange this.

## Getting started

### Prerequisites

- A working [Nix](https://nixos.org/download) installation with
  flakes enabled (`experimental-features = nix-command flakes` in
  `~/.config/nix/nix.conf`).
- A C++ toolchain and CMake (provided by the dev shell).
- Rust toolchain (provided by the dev shell).

Everything else is pinned in `flake.nix`. Enter the development
shell with:

```
nix develop
```

### Development setup

```
git clone https://github.com/anthropics/cargo-nix-plugin.git
cd cargo-nix-plugin
nix develop
```

### Running tests

```
# Rust resolver crate
( cd rust && cargo test )

# build-rust-crate replacement binary
( cd builder && cargo test )

# End-to-end Nix tests (sparse registry, git sources, mirrors,
# offline builds, crate2nix compatibility)
nix flake check
```

## How to contribute

### Reporting issues

Open an issue on GitHub. When reporting a bug, include steps to
reproduce, expected behavior, actual behavior, environment details
(`nix --version`, `uname -srm`, plugin commit), and relevant logs or
error messages — `--show-trace` output is especially useful for
evaluation failures.

### Submitting pull requests

1. Fork the repository and create a branch from `main`.
2. Make your changes, following the code style guidelines below.
3. Add or update tests as appropriate. Resolver behaviour changes
   should come with a corresponding `.nix` test under `tests/`.
4. Update documentation if your change affects public APIs or
   user-facing behavior.
5. Run the test suite and confirm all tests pass.
6. Open a pull request with a clear description of the change and
   its motivation.

## Code style

- **Rust** — `rustfmt` defaults; `cargo clippy --all-targets` clean.
  Prefer `thiserror` for library error types; avoid `.unwrap()`
  outside tests and provably-infallible call sites.
- **Nix** — match the surrounding file's formatting; `nixfmt-rfc-style`
  if you need a tie-breaker.
- **Copyright headers** — every new source file should carry the
  Apache-2.0 SPDX header used elsewhere in the tree.

## Review process

All pull requests require review from at least one maintainer before
merging. We aim to provide initial feedback within one week, though
response time may vary.

## Scope

This plugin replaces the generated `Cargo.nix` step from crate2nix
with a native `builtins.resolveCargoWorkspace` primop. Changes that
broaden that scope (e.g. resolving non-Cargo Rust projects, replacing
`buildRustCrate` itself) are likely to be declined — contributing
upstream to crate2nix or nixpkgs is usually a better fit.

If you're unsure whether a change is in scope, open an issue first
and let's talk before you write the code.

## Questions

Open a discussion on GitHub.
