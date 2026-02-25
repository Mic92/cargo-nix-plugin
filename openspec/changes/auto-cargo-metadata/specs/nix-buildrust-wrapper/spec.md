## MODIFIED Requirements

### Requirement: Wrapper auto-detects resolution mode
The Nix wrapper SHALL detect which mode to use based on parameter presence:
- If `metadata` is provided: pass `metadata` + `cargoLock` to the plugin (pure mode)
- If `metadata` is not provided: pass `manifestPath` (derived from `src`) + `cargoPath` to the plugin (subprocess mode)

#### Scenario: Simple usage without metadata
- **WHEN** user calls the wrapper with only `pkgs` and `src`
- **THEN** wrapper passes `manifestPath = "${src}/Cargo.toml"` and `cargoPath = "${pkgs.cargo}/bin/cargo"` to the plugin

#### Scenario: Explicit metadata usage
- **WHEN** user calls the wrapper with `metadata`, `cargoLock`, and `src`
- **THEN** wrapper passes `metadata` and `cargoLock` to the plugin, no `manifestPath` or `cargoPath`

### Requirement: cargoLock and metadata are optional parameters
The wrapper SHALL accept `metadata` and `cargoLock` as optional parameters (defaulting to null). The `src` parameter SHALL remain required.

#### Scenario: Minimal invocation
- **WHEN** user provides only `pkgs` and `src`
- **THEN** wrapper works without error, using subprocess mode
