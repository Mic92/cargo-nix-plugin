## Why

Nix-based Rust builds using crate2nix suffer from severe evaluation performance problems. crate2nix generates a `Cargo.nix` file that can be 50K-100K+ lines for large projects. The Nix evaluator must parse this enormous file, build an AST with thousands of thunks, and then execute recursive feature resolution (`mergePackageFeatures`, `expandFeatures`, `filterEnabledDependencies`) entirely in interpreted Nix — all before a single crate is built.

By moving Cargo.toml/Cargo.lock parsing and feature resolution into a Nix plugin (compiled native code), we eliminate the generated `Cargo.nix` file entirely and perform the expensive work orders of magnitude faster.

## What Changes

- New Nix plugin (shared library) providing a `builtins.resolveCargoWorkspace` primop that:
  - Parses `Cargo.toml` and `Cargo.lock` natively
  - Resolves the dependency graph and features in compiled code
  - Pre-evaluates `cfg()` target expressions for the requested platform
  - Returns a Nix attrset of crate metadata compatible with `buildRustCrate`
- Thin Nix wrapper (~50 lines) that takes the plugin output, applies `defaultCrateOverrides`, and calls `buildRustCrate` for each crate
- Eliminates the need for the `crate2nix generate` step and the generated `Cargo.nix` file
- Crate source hashes are extracted from `Cargo.lock` checksums (hex→nix-compatible format)

## Capabilities

### New Capabilities
- `cargo-workspace-resolver`: The core plugin builtin that parses Cargo manifests/lockfiles and returns resolved crate metadata as Nix values
- `nix-buildrust-wrapper`: Thin Nix-side glue that connects the plugin output to `buildRustCrate` with override support

### Modified Capabilities
<!-- None — this is a new project, not modifying existing capabilities -->

## Impact

- **Dependencies**: Nix plugin API (unstable, C++ ABI), Rust cargo_metadata/toml crates via C FFI shim
- **Build system**: Plugin must be compiled against matching Nix headers; distributed as a shared library
- **User workflow**: Users no longer run `crate2nix generate`; instead they configure `plugin-files` in nix.conf and use the builtin directly in their Nix expressions
- **Compatibility**: Must produce the same `buildRustCrate`-compatible attrset shape as crate2nix's `Cargo.nix` so existing `defaultCrateOverrides` work unchanged
