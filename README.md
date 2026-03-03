# cargo-nix-plugin

A Nix plugin that resolves Cargo workspaces natively, replacing the generated
`Cargo.nix` file from crate2nix with a single `builtins.resolveCargoWorkspace`
primop.

## What It Does

- Resolves Cargo workspaces at native speed — either by shelling out to
  `cargo metadata` automatically, or by parsing pre-provided metadata JSON
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

## Usage

### Simple (automatic)

Just point at your workspace root — the plugin calls `cargo metadata` for you:

```nix
cargoNix = cargo-nix-plugin.lib {
  inherit pkgs;
  src = ./.;  # must contain Cargo.toml + Cargo.lock
};
```

This shells out to `cargo metadata --format-version 1 --locked` during Nix
evaluation. It uses the user's `CARGO_HOME` cache, so it's near-instant when
dependencies have been fetched before. Network access is required on first use.

### Explicit (pure, offline)

For pure evaluation or CI without network, pre-generate the metadata:

```bash
cargo metadata --format-version 1 --locked > metadata.json
```

Then pass it explicitly:

```nix
cargoNix = cargo-nix-plugin.lib {
  inherit pkgs;
  metadata = builtins.readFile ./metadata.json;
  cargoLock = builtins.readFile ./Cargo.lock;
  src = ./.;
};
```

A helper is also available:

```bash
nix run .#generate-metadata -- > metadata.json
```

## Example

The plugin must be loaded by the same Nix version it was compiled against
(see [Compatibility](#compatibility)). Evaluate with the plugin loaded via
`--option`:

```bash
PLUGIN=$(nix build .#cargo-nix-plugin --print-out-paths)
NIX=$(nix build nixpkgs#nixVersions.nix_2_33 --print-out-paths | grep -v man)

$NIX/bin/nix-instantiate --eval \
  --option plugin-files "$PLUGIN/lib/nix/plugins/libcargo_nix_plugin.so" \
  -E '(import ./lib { pkgs = import <nixpkgs> {}; src = ./.; }).workspaceMembers'
```

Or permanently in `nix.conf` / `~/.config/nix/nix.conf` (only if your system
Nix matches the plugin's build version):

```ini
plugin-files = /path/to/libcargo_nix_plugin.so
```

### flake.nix

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    cargo-nix-plugin.url = "github:your-org/cargo-nix-plugin";
  };

  outputs = { self, nixpkgs, cargo-nix-plugin }:
    let
      pkgs = import nixpkgs { system = "x86_64-linux"; };

      cargoNix = cargo-nix-plugin.lib {
        inherit pkgs;
        src = ./.;
      };
    in {
      packages.x86_64-linux.default = cargoNix.rootCrate.build;
    };
}
```

## Clippy

The wrapper provides cached clippy checks via `cargoNix.clippy`. Dependencies
are compiled once with `rustc` and cached in the Nix store; only workspace
members are re-checked with `clippy-driver`. This means running clippy on a
large workspace is as fast as compiling just your local crates.

```nix
cargoNix = cargo-nix-plugin.lib {
  inherit pkgs;
  src = ./.;
};

# Check all workspace members
cargoNix.clippy.allWorkspaceMembers

# Check a single member
cargoNix.clippy.workspaceMembers.my-crate.build
```

To fail on warnings, pass extra clippy flags:

```nix
cargoNix = cargo-nix-plugin.lib {
  inherit pkgs;
  src = ./.;
  clippyArgs = [ "-D" "warnings" ];
};
```

### How clippy caching works

`clippy-driver` is a drop-in replacement for `rustc` — it accepts identical
command-line flags and produces the same artifacts, but also runs lint passes.
The wrapper creates a small shim package where `bin/rustc` calls
`clippy-driver`, and passes it as the `rust` override to `buildRustCrate` for
workspace members only. Non-workspace dependencies use the normal `rustc` and
resolve to the **exact same Nix store paths** as a regular build — no redundant
compilation.

## How It Works

1. **Nix plugin**: Adds a `builtins.resolveCargoWorkspace` primop to Nix. When
   you call `cargo-nix-plugin.lib { ... }`, this primop resolves your entire
   Cargo workspace — dependencies, features, platform-specific conditionals —
   and returns the crate graph as a Nix attrset. In automatic mode it shells
   out to `cargo metadata`; in explicit mode it parses pre-provided JSON.

2. **Nix wrapper**: Takes the resolved crate graph and
   builds each crate with `buildRustCrate`, wiring up dependencies
   automatically. Supports proc-macro cross-compilation, crate overrides,
   and the standard `workspaceMembers`/`rootCrate` interface.

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

- **Nix**: The plugin must be loaded by the **same Nix version** it was compiled
  against — the Nix plugin ABI is not stable across versions. If you see errors
  like `expected a set but found a set`, you have a version mismatch. The flake
  currently builds against Nix 2.33, so use Nix 2.33.x to evaluate:

  ```bash
  # Get the matching nix
  NIX=$(nix build nixpkgs#nixVersions.nix_2_33 --print-out-paths | grep -v man)
  PLUGIN=$(nix build .#cargo-nix-plugin --print-out-paths)

  $NIX/bin/nix build .#myPackage \
    --option plugin-files "$PLUGIN/lib/nix/plugins/libcargo_nix_plugin.so"
  ```

- **Platforms**: `x86_64-linux`, `aarch64-linux`, `aarch64-darwin`, and
  `x86_64-darwin`. Cross-compilation to other target platforms is supported.

- **buildRustCrate**: Compatible with nixpkgs `buildRustCrate` and
  `defaultCrateOverrides`

## License

MIT
