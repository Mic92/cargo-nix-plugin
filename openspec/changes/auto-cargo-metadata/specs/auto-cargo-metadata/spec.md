## ADDED Requirements

### Requirement: Plugin can run cargo metadata as subprocess
When `manifestPath` is provided instead of `metadata`, the Rust code SHALL execute `cargo metadata --format-version 1 --locked --manifest-path <manifestPath>` using the cargo binary at `cargoPath`, parse stdout as JSON, and use the result identically to an explicitly provided metadata string.

#### Scenario: Successful subprocess resolution
- **WHEN** plugin receives input with `manifestPath` pointing to a valid Cargo.toml and `cargoPath` pointing to a cargo binary
- **THEN** plugin runs cargo metadata, parses the output, and returns the same WorkspaceResult as if metadata JSON had been passed directly

#### Scenario: Cargo.lock read from manifest directory
- **WHEN** plugin runs in subprocess mode
- **THEN** plugin reads Cargo.lock from the same directory as the manifest path, without requiring an explicit `cargoLock` parameter

#### Scenario: Cargo subprocess failure
- **WHEN** cargo metadata fails (network error, missing lockfile, invalid manifest)
- **THEN** plugin returns an error containing cargo's stderr output

### Requirement: Explicit metadata still works
When `metadata` is provided, the plugin SHALL use it directly without spawning any subprocess, preserving full backward compatibility with the current pure evaluation path.

#### Scenario: Backward compatible explicit metadata
- **WHEN** plugin receives input with `metadata` (JSON string) and `cargoLock` (string)
- **THEN** plugin behaves identically to before this change — no subprocess, no filesystem reads

### Requirement: Exactly one input mode
The plugin SHALL require exactly one of `metadata` or `manifestPath`. Providing both or neither SHALL produce a clear error.

#### Scenario: Both metadata and manifestPath provided
- **WHEN** plugin receives input with both `metadata` and `manifestPath`
- **THEN** plugin returns an error indicating only one should be provided

#### Scenario: Neither metadata nor manifestPath provided
- **WHEN** plugin receives input with neither `metadata` nor `manifestPath`
- **THEN** plugin returns an error indicating one must be provided
