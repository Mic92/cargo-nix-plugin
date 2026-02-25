## MODIFIED Requirements

### Requirement: Plugin input contract
The FFI input struct SHALL accept the following fields:
- `metadata` (optional string): Explicit cargo metadata JSON
- `cargoLock` (optional string): Explicit Cargo.lock contents (required when `metadata` is provided)
- `manifestPath` (optional string): Path to Cargo.toml (triggers subprocess mode)
- `cargoPath` (optional string): Path to cargo binary (required when `manifestPath` is provided)
- `target` (required): Target platform description
- `rootFeatures` (optional, defaults to `["default"]`): Features to enable

#### Scenario: Subprocess mode with all required fields
- **WHEN** `manifestPath` and `cargoPath` are provided, `metadata` is null
- **THEN** plugin resolves workspace via cargo subprocess

#### Scenario: Explicit mode with all required fields
- **WHEN** `metadata` and `cargoLock` are provided, `manifestPath` is null
- **THEN** plugin resolves workspace by parsing the provided JSON directly

#### Scenario: Missing cargoPath in subprocess mode
- **WHEN** `manifestPath` is provided but `cargoPath` is null
- **THEN** plugin returns an error indicating cargoPath is required with manifestPath

#### Scenario: Missing cargoLock in explicit mode
- **WHEN** `metadata` is provided but `cargoLock` is null
- **THEN** plugin returns an error indicating cargoLock is required with metadata
