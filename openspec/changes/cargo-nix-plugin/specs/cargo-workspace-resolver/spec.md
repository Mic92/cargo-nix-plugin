## ADDED Requirements

### Requirement: Parse cargo metadata JSON
The builtin SHALL accept a `metadata` string attribute containing the JSON output of `cargo metadata --format-version 1 --locked` and parse it into the internal representation using the `cargo_metadata` crate's `Deserialize` implementation.

#### Scenario: Valid cargo metadata JSON
- **WHEN** the builtin receives a valid `cargo metadata` JSON string as the `metadata` attribute
- **THEN** it SHALL parse all packages, resolve nodes, workspace members, and the root package

#### Scenario: Invalid JSON
- **WHEN** the `metadata` attribute contains invalid JSON or JSON that does not match the cargo metadata format
- **THEN** the builtin SHALL throw a Nix evaluation error with a descriptive message

#### Scenario: Missing metadata attribute
- **WHEN** the `metadata` attribute is not provided
- **THEN** the builtin SHALL throw a Nix evaluation error indicating the required attribute is missing

### Requirement: Parse Cargo.lock for checksums
The builtin SHALL accept a `cargoLock` string attribute containing the contents of `Cargo.lock` and extract sha256 checksums for each package.

#### Scenario: Cargo.lock with checksums
- **WHEN** the `cargoLock` attribute contains a valid Cargo.lock with `checksum` fields
- **THEN** the builtin SHALL extract the hex-encoded sha256 for each package and convert them to SRI format (`sha256-<base64>`)

#### Scenario: Package without checksum
- **WHEN** a package in Cargo.lock has no `checksum` field (e.g., local path dependencies)
- **THEN** the builtin SHALL set the sha256 to null for that package

#### Scenario: Missing cargoLock attribute
- **WHEN** the `cargoLock` attribute is not provided
- **THEN** the builtin SHALL throw a Nix evaluation error indicating the required attribute is missing

### Requirement: Accept target platform description
The builtin SHALL accept a `target` attrset describing the target platform for resolving `cfg()` expressions on dependencies.

#### Scenario: Target with standard fields
- **WHEN** the `target` attribute is provided with fields `name`, `os`, `arch`, `vendor`, `env`, `family`, `pointer_width`, `endian`, `unix`, `windows`
- **THEN** the builtin SHALL use these fields to evaluate `cfg()` target expressions on dependencies

#### Scenario: Missing target attribute
- **WHEN** the `target` attribute is not provided
- **THEN** the builtin SHALL throw a Nix evaluation error indicating the required attribute is missing

### Requirement: Accept root features
The builtin SHALL accept an optional `rootFeatures` list of strings specifying which features to enable for root/workspace member crates.

#### Scenario: Explicit root features
- **WHEN** `rootFeatures` is provided as `[ "default" "feature-x" ]`
- **THEN** the builtin SHALL resolve features starting from the specified list for all workspace members

#### Scenario: Default root features
- **WHEN** `rootFeatures` is not provided
- **THEN** the builtin SHALL default to `[ "default" ]`

### Requirement: Resolve dependency graph with features
The builtin SHALL resolve the full dependency graph including feature unification, optional dependency activation, and transitive feature propagation, producing per-crate resolved feature lists.

#### Scenario: Feature unification across dependents
- **WHEN** crate A depends on crate C with feature "x" and crate B depends on crate C with feature "y", and both A and B are in the dependency graph
- **THEN** crate C's resolved features SHALL include both "x" and "y"

#### Scenario: Optional dependency activation via feature
- **WHEN** a crate has an optional dependency `foo` and feature `use-foo = ["dep:foo"]` is enabled
- **THEN** `foo` SHALL appear in that crate's resolved dependencies

#### Scenario: cfg()-conditional dependency filtering
- **WHEN** a dependency has `target = 'cfg(target_os = "linux")'` and the provided target has `os = "linux"`
- **THEN** the dependency SHALL be included in the resolved output

#### Scenario: cfg()-conditional dependency exclusion
- **WHEN** a dependency has `target = 'cfg(target_os = "windows")'` and the provided target has `os = "linux"`
- **THEN** the dependency SHALL be excluded from the resolved output

### Requirement: Return buildRustCrate-compatible crate attrset
The builtin SHALL return a Nix attrset containing `root`, `workspaceMembers`, and `crates` with a structure compatible with crate2nix's `buildRustCrateWithFeatures` / `buildRustCrate` interface.

#### Scenario: Crates.io dependency
- **WHEN** a crate is sourced from crates.io
- **THEN** its entry in `crates` SHALL include `crateName`, `version`, `edition`, `sha256` (SRI format), `dependencies`, `buildDependencies`, `devDependencies`, `features`, `resolvedDefaultFeatures`, `procMacro`, `build`, `libPath`, `libName`, `crateBin`, `libCrateTypes`, `links`, and `authors`

#### Scenario: Local path dependency
- **WHEN** a crate is a workspace member or local path dependency
- **THEN** its `source` field SHALL be `{ type = "local"; path = "<relative-path>"; }` and `sha256` SHALL be null

#### Scenario: Workspace with multiple members
- **WHEN** the cargo metadata describes a workspace with members "crate-a" and "crate-b"
- **THEN** `workspaceMembers` SHALL be an attrset mapping `"crate-a"` to its packageId and `"crate-b"` to its packageId, and `root` SHALL be null (pure workspace)

#### Scenario: Dependency with rename
- **WHEN** a dependency is renamed (e.g., `hyper-v0-14 = { package = "hyper", version = "0.14" }`)
- **THEN** the dependency entry SHALL include `rename = "hyper_v0_14"` alongside `name = "hyper"` and the correct `packageId`

#### Scenario: Proc-macro crate
- **WHEN** a crate has a `proc-macro` target kind
- **THEN** its entry SHALL have `procMacro = true`

### Requirement: Handle the crate2nix-torture workspace
The builtin SHALL successfully resolve the crate2nix-torture test workspace (224 workspace members, 1884 total crates, platform-conditional deps, version aliases, proc-macros) and produce output that, when fed through the Nix wrapper to `buildRustCrate`, builds successfully.

#### Scenario: Full torture test resolution
- **WHEN** the builtin receives cargo metadata and Cargo.lock from the crate2nix-torture workspace with a linux x86_64 target
- **THEN** it SHALL return a valid crates attrset with entries for all 1884 crates without errors

#### Scenario: Evaluation performance
- **WHEN** the builtin resolves the crate2nix-torture workspace
- **THEN** the Nix evaluation time (builtin call plus wrapper overhead) SHALL be significantly faster than evaluating the 99K-line Cargo.nix
