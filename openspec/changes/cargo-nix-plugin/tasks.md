## 1. Project Scaffolding

- [x] 1.1 Create flake.nix for the plugin with Nix 2.33 dev headers, Rust toolchain, and C++ compiler as build inputs
- [x] 1.2 Create the Rust library crate (`cargo-nix-plugin-core`) with `cargo_metadata`, `cargo-platform`, `toml`, `serde`, `serde_json`, `base64` dependencies and `crate-type = ["staticlib"]`
- [x] 1.3 Create the C++ shim with `plugin.cc` that includes Nix headers and registers a placeholder `resolveCargoWorkspace` primop
- [x] 1.4 Set up the Nix build that compiles the Rust staticlib, compiles the C++ shim, and links them into a `.so`
- [x] 1.5 Generate and commit `metadata.json` for the torture workspace — test fixture data for all subsequent tasks

## 2. Rust Core: Lockfile Parsing & Hash Conversion

- [x] 2.1 Write failing tests for `parse_lockfile()`: extracts known checksums by (name, version), returns empty for local crates, handles Cargo.lock v3/v4
- [x] 2.2 Implement `parse_lockfile()` and `hex_to_sri()`

## 3. Rust Core: cfg() Evaluation

- [x] 3.1 Write failing tests: `cfg(target_os = "linux")` matches linux, `cfg(windows)` doesn't, `cfg(all(...))`, `cfg(any(...))`, `cfg(not(...))`, bare target triples like `"aarch64-linux-android"`
- [x] 3.2 Implement cfg() evaluator using `cargo-platform` against a target description struct

## 4. Rust Core: Dependency Filtering

- [x] 4.1 Write failing tests: conditional dep included when cfg matches, excluded when it doesn't, optional dep excluded unless feature enables it, renamed deps resolved correctly
- [x] 4.2 Implement dependency filtering given dependencies, enabled features, and target

## 5. Rust Core: Feature Resolution

- [x] 5.1 Write failing tests: feature expansion (feature enables other features), feature unification across dependents, optional dep activation via `dep:` syntax, transitive propagation, `feature/subfeat` forwarding
- [x] 5.2 Implement `merge_package_features()`: walk dependency graph, merge features additively, expand feature rules, enable optional deps

## 6. Rust Core: Full Workspace Resolution

- [x] 6.1 Write failing integration test: `resolve_workspace()` on torture workspace metadata returns 1884 crates, 224 workspace members, spot-check specific crate features/deps/renames/procMacro/source types
- [x] 6.2 Implement `resolve_workspace(metadata, lockfile_hashes, target, root_features) -> WorkspaceResult` tying together parsing, resolution, and output generation

## 7. C FFI Bridge

- [x] 7.1 Define `extern "C"` interface and implement FFI entry point: parse inputs, call `resolve_workspace`, serialize result to JSON, return via out pointer
- [x] 7.2 Implement `free_string()` for C++ side

## 8. C++ Nix Plugin Shim

- [x] 8.1 Implement `prim_resolveCargoWorkspace`: extract attrs from Nix argument, serialize target to JSON, call Rust FFI, convert result JSON to Nix Values, register primop
- [x] 8.2 Write Nix eval test with torture test inputs: load plugin, call builtin, verify `crates` has 1884 entries, spot-check a specific crate's fields

## 9. Nix Wrapper

- [x] 9.1 Write `lib/default.nix`: wrapper function with `makeDefaultTarget`, source resolution (crates-io/local/git), derivation build graph with `builtByPackageId` memoization, proc-macro cross-compilation, crate renames, override support
- [x] 9.2 Expose `rootCrate`, `workspaceMembers`, `allWorkspaceMembers`

## 10. End-to-End: crate2nix-torture

- [x] 10.1 Write test flake.nix for torture workspace using plugin + wrapper with custom `buildRustCrateForPkgs` and injected native deps
- [x] 10.2 Verify `nix eval` produces derivations for all 224 workspace members without errors
- [ ] 10.3 Build at least one workspace member end-to-end
- [x] 10.4 Benchmark: `nix eval` time of plugin+wrapper vs 99K-line Cargo.nix

## 11. Documentation and Packaging

- [x] 11.1 Write README.md: what, install, generate metadata, example flake.nix
- [x] 11.2 Add plugin `.so` as flake output and `generate-metadata` app
