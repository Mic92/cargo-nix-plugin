## ADDED Requirements

### Requirement: Thin Nix wrapper connecting plugin output to buildRustCrate
The wrapper SHALL accept the output of `builtins.resolveCargoWorkspace` and a `buildRustCrateForPkgs` function, and produce a set of derivations by calling `buildRustCrate` for each crate with the correct dependencies wired as built derivations.

#### Scenario: Basic workspace build
- **WHEN** the wrapper receives the plugin output for a workspace and a `buildRustCrateForPkgs` function
- **THEN** it SHALL produce a `workspaceMembers` attrset where each member maps to a derivation built via `buildRustCrate`

#### Scenario: Dependency wiring
- **WHEN** crate A depends on crate B
- **THEN** the `dependencies` passed to `buildRustCrate` for crate A SHALL include the built derivation of crate B (not metadata)

#### Scenario: Proc-macro cross-compilation
- **WHEN** a dependency is a proc-macro crate and we are cross-compiling
- **THEN** the proc-macro crate SHALL be built for the build platform (via `pkgs.buildPackages`), not the host platform

### Requirement: Source resolution
The wrapper SHALL resolve each crate's source based on the `source` field from the plugin output.

#### Scenario: Crates.io source with sha256
- **WHEN** a crate has `source.type = "crates-io"` with a non-null `sha256`
- **THEN** the wrapper SHALL pass the `sha256` to `buildRustCrate`, which uses `fetchCrate` internally

#### Scenario: Local source with path
- **WHEN** a crate has `source.type = "local"` with `path = "./member-a"`
- **THEN** the wrapper SHALL resolve the path relative to a caller-provided `src` argument and apply source filtering

#### Scenario: Git source
- **WHEN** a crate has `source.type = "git"` with `url` and `rev`
- **THEN** the wrapper SHALL use `builtins.fetchGit { url = ...; rev = ...; }` to fetch the source

### Requirement: Crate override support
The wrapper SHALL support `defaultCrateOverrides` with the same semantics as crate2nix's existing override mechanism.

#### Scenario: Override adds native build inputs
- **WHEN** `defaultCrateOverrides` contains an entry for `openssl-sys` that adds `pkg-config` and `openssl` to build inputs
- **THEN** the `buildRustCrate` call for `openssl-sys` SHALL include those additional inputs

#### Scenario: Custom buildRustCrateForPkgs
- **WHEN** the caller provides a custom `buildRustCrateForPkgs` that injects native deps into every crate (as in the torture test's flake.nix)
- **THEN** every crate derivation SHALL use that customized builder

### Requirement: Interface compatibility with crate2nix flake pattern
The wrapper SHALL expose `workspaceMembers`, `rootCrate` (if applicable), and `allWorkspaceMembers` attributes matching the crate2nix `Cargo.nix` output interface, so existing flake.nix files can switch with minimal changes.

#### Scenario: Drop-in replacement for torture test flake.nix
- **WHEN** the torture test's flake.nix is adapted to use the plugin + wrapper instead of `./Cargo.nix`
- **THEN** the change SHALL be limited to replacing `pkgs.callPackage ./Cargo.nix { ... }` with a call to the wrapper, and the `packages` output SHALL work identically

#### Scenario: allWorkspaceMembers
- **WHEN** the wrapper is called for a workspace
- **THEN** `allWorkspaceMembers` SHALL be a derivation that symlink-joins all workspace member build outputs
