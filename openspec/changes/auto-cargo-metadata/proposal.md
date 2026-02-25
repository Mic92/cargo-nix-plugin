## Why

Users must manually run `cargo metadata --format-version 1 --locked` and pass the resulting 11MB+ JSON blob as a string through Nix evaluation. This is a clunky extra step that every user must remember, and it forces a large string through the Nix evaluator unnecessarily. The plugin should be able to call cargo directly when metadata isn't pre-provided.

## What Changes

- The Rust FFI layer gains an alternative input mode: instead of requiring a `metadata` JSON string, it can accept a `manifestPath` and a `cargoPath`, then shell out to `cargo metadata` internally
- When `metadata` is provided, behavior is identical to today (pure, no subprocess)
- When `metadata` is omitted, the plugin runs `cargo metadata --format-version 1 --locked --manifest-path <path>` and reads `Cargo.lock` from the manifest directory
- The Nix wrapper (`lib/default.nix`) is updated to support both modes, passing `cargoPath` (a Nix store path to the cargo binary) when metadata is not explicitly provided
- The plugin Nix package (`nix/plugin.nix`) gains `cargo` as a build-time input so the path can be baked in or passed through
- `cargoLock` becomes optional when using manifest-path mode (read from disk instead)

## Capabilities

### New Capabilities
- `auto-cargo-metadata`: Automatic cargo metadata resolution via subprocess when explicit metadata JSON is not provided

### Modified Capabilities
- `cargo-workspace-resolver`: The plugin input contract changes to accept either explicit metadata or a manifest path
- `nix-buildrust-wrapper`: The Nix wrapper gains optional `metadata`/`cargoLock` parameters and auto-detection via `src`

## Impact

- **Rust code**: `ffi.rs` input struct changes (new optional fields), new subprocess execution path in resolve logic
- **Nix wrapper**: `lib/default.nix` parameter handling changes, `cargoLock` and `metadata` become optional
- **Plugin packaging**: `nix/plugin.nix` may need to reference cargo store path
- **C++ shim**: No changes needed (already passes arbitrary attrset as JSON)
- **User API**: Non-breaking — existing explicit metadata usage continues to work, new simpler API is additive
- **Dependencies**: No new Rust crate dependencies (uses `std::process::Command`)
