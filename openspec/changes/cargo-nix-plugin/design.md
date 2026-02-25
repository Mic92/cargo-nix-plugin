## Context

crate2nix currently works as a two-phase system: (1) a Rust binary parses `Cargo.toml`/`Cargo.lock` via `cargo metadata` and generates a massive `Cargo.nix` file containing all crate metadata and resolution logic, then (2) the Nix evaluator parses this file and runs feature resolution in pure Nix to produce derivations via `buildRustCrate`.

The generated `Cargo.nix` for large projects can exceed 100K lines. The Nix evaluator spends significant time parsing this file, building AST nodes, and executing the recursive `mergePackageFeatures`/`expandFeatures`/`filterEnabledDependencies` functions in interpreted Nix. This also requires a `crate2nix generate` step that must be re-run whenever dependencies change.

The Nix plugin API (`plugin-files` in nix.conf) allows loading shared libraries that register new builtins via `RegisterPrimOp`. Plugins can construct Nix `Value*` objects directly (attrsets, lists, strings, etc.) using the evaluator's internal API. Target: Nix 2.33.

crate2nix's Rust codebase (`resolve.rs`, `lock.rs`, `metadata.rs`) already contains all the logic needed to resolve dependencies, compute features, and evaluate `cfg()` target expressions — all on top of `cargo_metadata` JSON output. The `cargo_metadata` crate can deserialize this JSON from a string without running cargo as a subprocess.

## Goals / Non-Goals

**Goals:**
- Eliminate the generated `Cargo.nix` file and the per-crate Nix evaluation overhead
- Move feature resolution from Nix evaluation into compiled Rust code
- Produce a crate metadata attrset compatible with the existing `buildRustCrate` infrastructure
- Support crate overrides (`defaultCrateOverrides`) for adding native dependencies
- Support cross-compilation by accepting a target platform description
- Extract sha256 hashes from `Cargo.lock` checksums without requiring `nix-prefetch`
- Support workspaces from the start

**Non-Goals:**
- Replacing `buildRustCrate` itself — we return metadata, not derivations
- Supporting Nix versions other than 2.33 initially
- Handling alternative registries or custom Nix sources (initial version focuses on crates.io + local + git)
- Eliminating the `cargo metadata` step entirely (user still runs `cargo metadata` once)

## Decisions

### Decision 1: Accept `cargo metadata` JSON as input, not raw Cargo.toml/Cargo.lock

**Choice**: The builtin accepts the output of `cargo metadata --format-version 1 --locked` as a JSON string, plus the `Cargo.lock` content for sha256 checksums.

**Alternatives considered**:
- *Parse Cargo.toml + Cargo.lock directly*: `Cargo.lock` has the resolved dependency graph but lacks per-crate metadata (features map, edition, targets, platform conditions, proc_macro). That metadata lives in each crate's `Cargo.toml` or the crates.io index. Reimplementing cargo's resolution from scratch is a massive undertaking.
- *Run `cargo metadata` from within the plugin*: Makes the builtin impure — unacceptable during Nix evaluation.
- *Use cargo as a Rust library*: The `cargo` crate is enormous, tightly coupled to specific rustc versions, and not designed for stable library use.

**Rationale**: The `cargo_metadata` crate can `Deserialize` the JSON from a string. This gives us the fully resolved dependency graph, all package metadata, feature maps, target conditions, and editions — everything crate2nix uses today. The user runs `cargo metadata` once (outside Nix, or in a helper script) and passes the result as a string. This is pure from Nix's perspective.

The `Cargo.lock` is needed separately for sha256 checksums (the `checksum` field), which `cargo metadata` doesn't include.

### Decision 2: Plugin returns crate metadata attrset, not derivations

**Choice**: The builtin returns a Nix attrset matching the shape of crate2nix's `internal.crates` — the same structure that `buildRustCrate` consumes. A thin Nix wrapper calls `buildRustCrate`.

**Alternatives considered**:
- *Generate derivations directly*: Would require reimplementing all of `buildRustCrate` (rustc flag generation, cross-compilation, build phases, crate overrides) inside the plugin. Tightly couples the plugin to nixpkgs internals.
- *Return raw parsed metadata*: Too low-level; Nix code would still need to do feature resolution.

**Rationale**: Clean separation of concerns. The plugin owns parsing + resolution (the expensive part). Nix owns build orchestration (where nixpkgs flexibility matters). Overrides work unchanged.

### Decision 3: Pre-resolve cfg() target expressions in the plugin

**Choice**: The builtin accepts a target description attrset (`{ os, arch, vendor, env, ... }`) and evaluates `cfg()` expressions at resolution time. Only dependencies matching the target are included. No Nix lambdas needed in the output.

**Alternatives considered**:
- *Return cfg() as Nix lambdas*: Constructing Nix lambdas from C++ is fragile and complex.
- *Return cfg() as structured data*: Requires a Nix-side filter function, adding evaluation cost.

**Rationale**: Pre-resolving is simplest and eliminates the most Nix evaluation. Cross-compilation is supported by calling the builtin with different target attrsets for build vs host platform, mirroring how crate2nix's `mkBuiltByPackageIdByPkgs` already works.

### Decision 4: C++ shim with Rust core via C FFI

**Choice**: Thin C++ plugin entry point registers the primop and constructs Nix `Value*` objects. The Rust static library (linked in) does all parsing and resolution, returning results via `extern "C"` functions as a C-friendly data structure (or JSON) that the C++ side converts to Nix values.

**Alternatives considered**:
- *Pure C++*: Would need to rewrite Cargo.toml/Cargo.lock parsing and resolution from scratch.
- *Pure Rust*: Cannot easily call Nix internal APIs (`state.buildBindings`, `v.mkString`, etc.) without extensive bindings to unstable C++ interfaces.

**Rationale**: Leverages crate2nix's existing resolution logic (the `cargo_metadata`, `cargo-platform` crates and the resolution algorithms in `resolve.rs`). The C++ shim is ~200-300 lines. The Rust side can be tested independently.

### Decision 5: SRI hash format

**Choice**: Convert `Cargo.lock` hex sha256 checksums to SRI format (`sha256-<base64>`).

**Rationale**: SRI is the modern Nix standard (supported since 2.4), simpler than nix-base32.

### Decision 6: Builtin interface

```nix
builtins.resolveCargoWorkspace {
  # Required: output of `cargo metadata --format-version 1 --locked`
  metadata = builtins.readFile ./metadata.json;
  # Required: contents of Cargo.lock (for sha256 checksums)
  cargoLock = builtins.readFile ./Cargo.lock;
  # Required: target platform for cfg() resolution
  target = {
    name = "x86_64-unknown-linux-gnu";
    os = "linux"; arch = "x86_64"; vendor = "unknown";
    env = "gnu"; family = ["unix"];
    pointer_width = "64"; endian = "little";
    unix = true; windows = false;
  };
  # Optional: features to enable for root/workspace members
  rootFeatures = [ "default" ];
}
```

Returns:
```nix
{
  root = "my-crate";           # packageId of root, or null
  workspaceMembers = {         # name → packageId
    "my-crate" = "my-crate";
  };
  crates = {
    "my-crate" = {
      crateName = "my-crate"; version = "0.1.0"; edition = "2021";
      sha256 = "sha256-...";    # SRI from Cargo.lock; null for local crates
      src = null;               # null = use sha256 with fetchurl; set for local/git
      dependencies = [
        { name = "tokio"; packageId = "tokio 1.35.0";
          usesDefaultFeatures = true; features = []; }
      ];
      buildDependencies = [ ... ];
      devDependencies = [ ... ];
      features = { default = [ "rt" "net" ]; rt = []; net = []; };
      resolvedDefaultFeatures = [ "default" "rt" "net" ];
      procMacro = false;
      build = "build.rs";       # or null
      libPath = "src/lib.rs";   # or null
      libName = "my_crate";     # or null (defaults to crateName)
      crateBin = [ { name = "my-crate"; path = "src/main.rs"; } ];
      libCrateTypes = [ "lib" ];
      links = null;
      authors = [ "..." ];
    };
    "tokio 1.35.0" = { ... };
  };
}
```

## Risks / Trade-offs

- **[Nix plugin API instability]** → Pin to Nix 2.33. The C++ shim is thin, limiting ABI blast radius. Test in CI.

- **[Feature resolution divergence]** → Must match cargo's behavior and crate2nix's Nix-side resolution. Mitigation: reuse crate2nix's tested Rust resolution code. Integration tests comparing output against crate2nix-generated `Cargo.nix` for known projects.

- **[cargo metadata as extra step]** → Users must generate `metadata.json` before Nix eval. Mitigation: provide a helper script or flake app. This is strictly less friction than running `crate2nix generate` today.

- **[Git dependency source handling]** → Git sources need `fetchgit` with sha256. The plugin returns the git URL + rev; the Nix wrapper handles fetching. For initial version, git source hashes may need prefetching.

- **[Loss of laziness]** → Plugin resolves all crates eagerly. Mitigation: the speed of compiled resolution more than compensates. Most builds need the full graph anyway.

- **[Build complexity]** → Plugin must compile against Nix 2.33 headers and link correctly. Mitigation: Nix flake with CI builds the plugin.
