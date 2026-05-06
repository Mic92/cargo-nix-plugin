# cargo-nix-plugin

A Nix plugin that resolves Cargo workspaces natively, replacing the generated
`Cargo.nix` file from crate2nix with a single `builtins.resolveCargoWorkspace`
primop.

## What It Does

- Resolves Cargo workspaces at native speed — directly from `Cargo.lock` and
  the sparse registry index, no `cargo` binary required (or, optionally, from
  pre-generated `cargo metadata` JSON)
- Pre-evaluates `cfg()` target expressions for the requested platform
- Returns a Nix attrset compatible with `buildRustCrate`
- Eliminates the `crate2nix generate` step and the 50K-100K line `Cargo.nix`

## Install

Add the plugin to your Nix configuration:

```nix
# nix.conf or via --option — point at the directory so the right
# extension (.so/.dylib) is picked up automatically
plugin-files = /path/to/cargo-nix-plugin/lib/nix/plugins
```

Or use the flake output:

```nix
{
  inputs.cargo-nix-plugin.url = "github:anthropics/cargo-nix-plugin";
}
```

## Usage

### Default (lockfile resolve)

Just point at your workspace root:

```nix
cargoNix = cargo-nix-plugin.lib {
  inherit pkgs;
  src = ./.;  # must contain Cargo.toml + Cargo.lock
};
```

The plugin reads `Cargo.lock` plus the sparse registry index directly — no
`cargo` binary, no crate sources at eval time. On first use it fetches each
crate's index entry (a few hundred bytes) into `$CARGO_HOME` and reuses it
thereafter.

If your environment already redirects cargo to a mirror, the resolver follows
the same configuration — `CARGO_REGISTRIES_CRATES_IO_INDEX` or
`[source.crates-io] replace-with` in `.cargo/config.toml` — so no
plugin-specific setup is required:

```toml
# .cargo/config.toml — honoured by both cargo and the plugin
[source.crates-io]
replace-with = "mirror"
[source.mirror]
registry = "sparse+https://artifactory.example/api/cargo/crates/index/"
```

If every index lookup fails (e.g. egress to `index.crates.io` is blocked and
no mirror is configured), evaluation fails loudly rather than silently
producing derivations with missing features.

### Explicit metadata

Alternatively, pre-generate cargo's resolution and pass it in:

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

### Warming the index cache out of band

In the rare case where the evaluating host has *no* reachable index at all,
`cargo-nix-prefetch` can populate `$CARGO_HOME` ahead of time on a connected
host (it observes the same mirror precedence as the plugin):

```bash
nix run .#cargo-nix-prefetch -- --manifest-path ./Cargo.toml
nix run .#cargo-nix-prefetch -- --manifest-path ./Cargo.toml --check   # verify
```

Use `--output DIR` to write into a fresh directory instead of the ambient
`$CARGO_HOME`, then point the resolver at it explicitly:

```bash
nix run .#cargo-nix-prefetch -- --manifest-path ./Cargo.toml --output ./.cargo-index
```

```nix
cargoNix = cargo-nix-plugin.lib {
  inherit pkgs;
  src = ./.;
  cargoHome = ./.cargo-index;   # pre-warmed by cargo-nix-prefetch
};
```

The same shape works wrapped in a fixed-output derivation if you want the
cache pinned by hash rather than checked in.

### Git dependencies

`git+…` entries in `Cargo.lock` are fetched at eval time with
`builtins.fetchGit { url; rev; allRefs = true; submodules = true; }` so the
resolver can read each crate's `Cargo.toml` (the registry index has no
record of them). Submodules are pulled to match cargo, which always
recurses them for git deps. When the upstream repo is a Cargo workspace,
the resolver locates the right member and passes its sub-directory to
`buildRustCrate` as `workspace_member`.

Override `gitSources` when `fetchGit` can't reach the repo (private auth,
vendored fixture), to pin a `narHash`/use a FOD fetcher, or to skip
submodules for a repo that doesn't need them:

```nix
cargoNix = cargo-nix-plugin.lib {
  inherit pkgs;
  src = ./.;
  gitSources = {
    # key = "${url}#${rev}" with git+ and ?query stripped — exactly what
    # appears in Cargo.lock after `git+` and before `?`, plus `#REV`.
    "https://github.com/Byron/gitoxide#abcdef…" = pkgs.fetchgit {
      url = "git@github.com:Byron/gitoxide";
      rev = "abcdef…";
      hash = "sha256-…";
    };
  };
};
```

A `git+` source without a pinned `#rev` is rejected; `Cargo.lock` always
pins one.

### Debug logging

The resolver stays quiet on the happy path so eval output isn't drowned in
progress noise. Set `CARGO_NIX_DEBUG=1` to surface the informational logs
(mirror selection, index prefetch timings, per-crate retry attempts) on
stderr. Warnings about misconfiguration and hard errors are always printed
regardless of this flag.

## Example

The plugin must be loaded by the same Nix version it was compiled against
(see [Compatibility](#compatibility)). Evaluate with the plugin loaded via
`--option`:

```bash
PLUGIN=$(nix build .#cargo-nix-plugin --print-out-paths)
NIX=$(nix build nixpkgs#nixVersions.nix_2_34 --print-out-paths | grep -v man)

$NIX/bin/nix-instantiate --eval \
  --option plugin-files "$PLUGIN/lib/nix/plugins" \
  -E '(import ./lib { pkgs = import <nixpkgs> {}; src = ./.; }).workspaceMembers'
```

Or permanently in `nix.conf` / `~/.config/nix/nix.conf` (only if your system
Nix matches the plugin's build version):

```ini
plugin-files = /path/to/cargo-nix-plugin/lib/nix/plugins
```

### flake.nix

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    cargo-nix-plugin.url = "github:anthropics/cargo-nix-plugin";
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

## Tests

```nix
checks.x86_64-linux.my-crate-tests =
  cargoNix.workspaceMembers.my-crate.runTests;
```

`runTests` compiles lib unit tests and integration tests under `tests/`
(with `[dev-dependencies]` wired in) and runs them sequentially. The regular
`.build` derivation is unchanged. Integration tests can spawn the crate's
binaries via `env!("CARGO_BIN_EXE_<name>")` exactly as under `cargo test`.

Tests that shell out to external tools at runtime declare them via
`nativeCheckInputs` in `crateOverrides`; `runTests` puts them on PATH:

```nix
cargoNix = cargo-nix-plugin.lib {
  inherit pkgs;
  src = ./.;
  crateOverrides = pkgs.defaultCrateOverrides // {
    my-crate = _: { nativeCheckInputs = [ pkgs.sqlite ]; };
  };
};
```

The runner sets `RUST_BACKTRACE=1` and points `CARGO_TARGET_TMPDIR` at a
fresh temp dir. If you need different behaviour (test filters, `--nocapture`,
a custom harness), the compiled artefacts are at `.buildTests` —
`$out/tests/*` are the test executables, `$out/bin/*` the real binaries —
and `runTests.passthru.testsDrv` points there too.

Known limitations: doctests are not built, per-`[[bin]]` unit tests are not
compiled, and tests under `examples/` / `benches/` are not discovered.

## How It Works

1. **Nix plugin**: Adds a `builtins.resolveCargoWorkspace` primop to Nix. When
   you call `cargo-nix-plugin.lib { ... }`, this primop resolves your entire
   Cargo workspace — dependencies, features, platform-specific conditionals —
   and returns the crate graph as a Nix attrset. In the default mode it reads
   `Cargo.lock` and the sparse registry index directly; in explicit mode it
   parses pre-provided `cargo metadata` JSON.

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

## Custom cfgs

To set custom cfgs during `[target.'cfg(...)']` dependency resolution
(equivalent to `RUSTFLAGS="--cfg foo"` at cargo-metadata time), pass
`extraCfgs`:

```nix
extraCfgs = [ "my_platform" ];
```

Pair with passing the same `--cfg` via rustc opts so `#[cfg(foo)]` in source
compiles too — `extraCfgs` only affects dependency resolution.

## Compatibility

- **Nix**: The plugin must be loaded by the **same Nix version** it was compiled
  against — the Nix plugin ABI is not stable across versions. If you see errors
  like `expected a set but found a set`, you have a version mismatch.
  `.#cargo-nix-plugin` (the default) is built against Nix 2.34, so use Nix
  2.34.x to evaluate:

  ```bash
  # Get the matching nix
  NIX=$(nix build nixpkgs#nixVersions.nix_2_34 --print-out-paths | grep -v man)
  PLUGIN=$(nix build .#cargo-nix-plugin --print-out-paths)

  $NIX/bin/nix build .#myPackage \
    --option plugin-files "$PLUGIN/lib/nix/plugins"
  ```

  For other Nix versions, build the matching per-version attribute, e.g.
  `.#cargo-nix-plugin-nix_2_31` to pair with `nixVersions.nix_2_31`. The
  flake's `nixVersions` set (in `flake.nix`) lists what's currently built;
  Nix >= 2.30 is required.

- **Platforms**: `x86_64-linux`, `aarch64-linux`, and `aarch64-darwin`.
  Cross-compilation to other target platforms is supported.

- **API level**: `lib/` checks that the loaded plugin speaks the same
  contract version before resolving and warns on mismatch (e.g. when the
  plugin baked into your Nix lags the `lib/` checkout). The wrapper
  result exposes both sides so you can turn that into a hard failure:

  ```nix
  let cargoNix = import ./lib { inherit pkgs; src = ./.; }; in
  assert cargoNix.apiLevel == cargoNix.resolverApiLevel;
  cargoNix.workspaceMembers
  ```

  `apiLevel` is what this `lib/` speaks; `resolverApiLevel` is what the
  loaded plugin reports (0 if the plugin predates the check).

- **buildRustCrate**: Compatible with nixpkgs `buildRustCrate` and
  `defaultCrateOverrides`

## Status

Maintained by Anthropic. Provided AS IS without warranty (see LICENSE).
We triage issues and review pull requests but do not commit to fixing every
bug or accepting every feature request. For security issues, see
`SECURITY.md`.

## License

Apache License 2.0. See [LICENSE](LICENSE).
