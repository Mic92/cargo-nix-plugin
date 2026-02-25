## Context

Currently the plugin accepts a `metadata` JSON string (output of `cargo metadata --format-version 1 --locked`) passed through Nix evaluation. The user must run cargo metadata externally, capture the output, and feed it in. The Rust code deserializes this into `cargo_metadata::Metadata`, reshapes it for `buildRustCrate`, and returns JSON that the C++ shim converts to Nix values.

The cargo binary already exists in nixpkgs and can be referenced as a store path. The plugin runs as native code inside the Nix evaluator with no sandbox restrictions — it can spawn subprocesses and do network I/O.

## Goals / Non-Goals

**Goals:**
- Allow users to omit `metadata` and `cargoLock` — the plugin shells out to cargo automatically
- Keep backward compatibility: explicit `metadata` + `cargoLock` still works identically
- Cargo binary path is a Nix store path, not reliant on `PATH`

**Non-Goals:**
- Linking cargo as a Rust library (too heavy, unstable API)
- Eliminating the JSON intermediate format between cargo and resolve_workspace (still needed internally when shelling out)
- Making this work under `--pure-eval` (subprocess needs network for cold CARGO_HOME)

## Decisions

### 1. Subprocess via `std::process::Command`, not cargo-as-library

Cargo published as a crate has 500+ transitive dependencies, an explicitly unstable API, and pulls in git2, curl, openssl, etc. Shelling out to the cargo binary is simpler, has zero additional Rust deps, and uses the exact same code path users already rely on.

**Alternative**: Link `cargo` crate directly. Rejected due to massive dependency tree, API instability, and complex initialization (GlobalContext, registry setup).

### 2. Two-mode input: `metadata` (string) vs `manifestPath` (path)

The `PluginInput` struct gets both fields as `Option`. Exactly one must be provided:
- `metadata: Some(json)` → current path, parse directly
- `manifestPath: Some(path)` → new path, run cargo subprocess

This keeps the FFI interface a single function with a single JSON input attrset.

**Alternative**: Separate builtins (`resolveCargoWorkspace` vs `resolveCargoWorkspaceFromPath`). Rejected — unnecessary complexity, single entry point with optional fields is cleaner.

### 3. `cargoPath` passed from Nix, not discovered from PATH

The Nix wrapper passes `cargoPath = "${pkgs.cargo}/bin/cargo"` to the plugin. This guarantees a known cargo version regardless of the user's environment.

**Alternative**: Search `PATH`. Rejected — unreliable in Nix eval context, version mismatches.

### 4. `cargoLock` auto-read from manifest directory in subprocess mode

When shelling out, the Rust code reads `Cargo.lock` from `${manifestDir}/Cargo.lock` directly via `std::fs::read_to_string`. No need for the user to pass it separately. In explicit metadata mode, `cargoLock` is still required as a parameter.

### 5. Nix wrapper detects mode based on `metadata` parameter presence

```
if metadata != null → pass metadata + cargoLock to plugin (current)
else               → pass manifestPath + cargoPath to plugin (new)
```

`src` is always required (used for local crate source resolution). `cargoLock` and `metadata` become optional — only needed for the explicit/pure path.

## Risks / Trade-offs

- **[Cold CARGO_HOME]** → First invocation downloads registry index + crate tarballs. Mitigated: this is the same cost as running `cargo metadata` manually. Subsequent evals hit cache. Users who need pure eval can still pass explicit metadata.

- **[Subprocess in Nix eval]** → Unusual pattern, may surprise users expecting pure evaluation. Mitigated: clearly documented, explicit metadata path remains available as pure alternative.

- **[cargo version coupling]** → Plugin depends on `cargo metadata` JSON format stability. Mitigated: format version 1 has been stable for years, and we pin the cargo version via Nix.

- **[Error reporting]** → If cargo fails (network error, missing lockfile), the error surfaces through the plugin. Mitigated: capture stderr from cargo and include it in the Nix eval error message.
