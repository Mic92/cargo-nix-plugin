# cargo-nix-plugin

A Nix plugin that resolves Cargo workspaces natively, replacing the generated
`Cargo.nix` file from crate2nix with a single `builtins.resolveCargoWorkspace`
primop.

## What It Does

- Parses `cargo metadata` JSON and `Cargo.lock` in compiled Rust code
- Resolves the dependency graph and features at native speed
- Pre-evaluates `cfg()` target expressions for the requested platform
- Returns a Nix attrset compatible with `buildRustCrate`
- Eliminates the `crate2nix generate` step and the 50K-100K line `Cargo.nix`

## Install

Add the plugin to your Nix configuration:

```nix
# nix.conf or via --option
plugin-files = /path/to/libcargo_nix_plugin.so
```

Or use the flake output:

```nix
{
  inputs.cargo-nix-plugin.url = "github:your-org/cargo-nix-plugin";
}
```

## Generate Metadata

Run once whenever dependencies change:

```bash
cargo metadata --format-version 1 --locked > metadata.json
```

Or use the provided helper:

```bash
nix run .#generate-metadata -- > metadata.json
```

## Example flake.nix

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    cargo-nix-plugin.url = "github:your-org/cargo-nix-plugin";
  };

  outputs = { self, nixpkgs, cargo-nix-plugin }:
    let
      pkgs = import nixpkgs {
        system = "x86_64-linux";
        config.plugin-files =
          "${cargo-nix-plugin.packages.x86_64-linux.cargo-nix-plugin}/lib/nix/plugins/libcargo_nix_plugin.so";
      };

      cargoNix = import "${cargo-nix-plugin}/lib" {
        inherit pkgs;
        metadata = builtins.readFile ./metadata.json;
        cargoLock = builtins.readFile ./Cargo.lock;
        src = ./.;
      };
    in {
      packages.x86_64-linux.default = cargoNix.rootCrate.build;
    };
}
```

## How It Works

1. **C++ plugin shim** (~50 lines): Registers `builtins.resolveCargoWorkspace`,
   serializes the Nix input attrset to JSON via `printValueAsJSON`, calls the
   Rust FFI, and converts the JSON result back to Nix values via `parseJSON`.

2. **Rust core** (~600 lines): Parses `cargo metadata` JSON using the
   `cargo_metadata` crate, extracts `Cargo.lock` checksums (hex → SRI),
   evaluates `cfg()` expressions using `cargo-platform`, resolves dependencies,
   and returns the full crate graph as JSON.

3. **Nix wrapper** (`lib/default.nix`): Takes the plugin output and calls
   `buildRustCrate` for each crate with dependencies wired as built derivations.
   Supports proc-macro cross-compilation, crate overrides, and the standard
   `workspaceMembers`/`rootCrate` interface.

## Target Platform

The plugin accepts a target description attrset:

```nix
target = {
  name = "x86_64-unknown-linux-gnu";
  os = "linux"; arch = "x86_64"; vendor = "unknown"; env = "gnu";
  family = ["unix"]; pointer_width = "64"; endian = "little";
  unix = true; windows = false;
};
```

The wrapper auto-detects this from `stdenv.hostPlatform`.

## Compatibility

- **Nix**: 2.33+ (plugin API)
- **Rust**: Uses `cargo_metadata` 0.18, `cargo-platform` 0.1
- **buildRustCrate**: Compatible with nixpkgs `buildRustCrate` and
  `defaultCrateOverrides`

## License

MIT
