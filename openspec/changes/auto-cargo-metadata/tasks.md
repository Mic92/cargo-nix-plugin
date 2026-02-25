## 1. Rust FFI Input Changes

- [x] 1.1 Update `PluginInput` struct in `ffi.rs`: make `metadata` and `cargo_lock` optional, add optional `manifest_path` and `cargo_path` fields
- [x] 1.2 Add input validation: exactly one of `metadata` or `manifest_path` must be provided, with their respective required companions (`cargo_lock` with `metadata`, `cargo_path` with `manifest_path`)

## 2. Cargo Subprocess Execution

- [x] 2.1 Add `run_cargo_metadata` function: takes `cargo_path` and `manifest_path`, runs `cargo metadata --format-version 1 --locked --manifest-path <path>`, returns stdout as String or error with stderr
- [x] 2.2 Add `Cargo.lock` file reading: read from manifest directory when in subprocess mode
- [x] 2.3 Wire up the two-mode dispatch in `resolve_cargo_workspace` FFI function: if `metadata` is present use it directly, otherwise call `run_cargo_metadata` and read `Cargo.lock` from disk

## 3. Nix Wrapper Updates

- [x] 3.1 Make `metadata` and `cargoLock` parameters optional (default to null) in `lib/default.nix`
- [x] 3.2 Add mode detection: if `metadata` is null, pass `manifestPath` and `cargoPath` to the plugin instead of `metadata` and `cargoLock`
- [x] 3.3 Set `cargoPath` to `"${pkgs.cargo}/bin/cargo"` in the wrapper when using subprocess mode

## 4. Tests

- [x] 4.1 ~Add Rust unit test for input validation~ — dropped, trivially correct match arms aren't worth testing
- [x] 4.2 Add Rust `#[ignore]` integration test that runs subprocess on this crate's own workspace and verifies output
- [x] 4.3 Verify existing Nix eval tests still pass with explicit metadata path (backward compatibility — these stay on explicit metadata since they run without network)

## 5. Cleanup

- [x] 5.1 Update `lib/default.nix` usage comment at the top to document both modes
- [x] 5.2 Remove or mark `generate-metadata` app in `flake.nix` as optional/legacy
