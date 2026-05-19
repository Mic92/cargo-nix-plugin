# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# Dynamic-derivations mode: evaluate without the cargo-nix plugin
# loaded, using a build-time planner derivation instead of an eval-time
# primop.
#
# Architecture:
#
#   workspaceMembers.<name>.build
#     = wrap (builtins.outputOf (planner <name>) "out")
#
#   planner <name> [recursive-nix, CA text output]:
#     1. cargo metadata --offline (against importCargoLock vendor dir)
#     2. cargo-nix-resolve  (metadata mode) -> resolved.json
#     3. nix-instantiate ./dyn-drv-eval.nix -A workspaceMembers.<name>.build
#     4. cp <drvPath> $out
#
# Step 3 re-enters lib/default.nix with `resolvedJson` set, so the
# buildRustCrate wiring (crate graph, splicing, overrides) is the same
# Nix code as the plugin path — only the resolve step moves from eval
# time to build time.
#
# Requires experimental features on the evaluating user AND the builder:
#   experimental-features = nix-command flakes ca-derivations dynamic-derivations
#   system-features       = recursive-nix
#
# Reduced interface vs. plugin path:
#   - workspaceMembers.<name>.build / .buildTests / .runTests
#   - allWorkspaceMembers, clippy.allWorkspaceMembers, clippy.workspaceMembers
#   - no caller-side `.override` on the produced drv (it is a deferred
#     placeholder, not a derivation attrset)
#   - `crateOverrides` must be a *path* to a Nix file, not an inline
#     function — closures do not cross the planner-derivation boundary.
#     Pass `crateOverridesFile = ./overrides.nix` (file: pkgs: { ... }).
{
  pkgs,
  lib,
  stdenv,
  apiLevel,
  # Original argument set the user passed to lib/default.nix. Re-threaded
  # into the inner eval (sans the bits that cannot be serialized).
  libArgs,
  # Pre-computed target description (so planner & inner eval agree).
  target,
}:

let
  inherit (libArgs) src;

  rootFeatures = libArgs.rootFeatures or [ ];
  noDefaultFeatures = libArgs.noDefaultFeatures or false;
  clippyArgs = libArgs.clippyArgs or [ ];
  extraCfgs = libArgs.extraCfgs or [ ];
  # crateOverrides closures cannot be sent into the planner build; require
  # a file path instead. Surfaced as a hard error to avoid silent drift.
  crateOverridesFile = libArgs.crateOverridesFile or null;

  haveDynDrv = builtins ? outputOf;
  guard =
    if (libArgs.crateOverrides or null) != null then
      throw ''
        cargo-nix-plugin: dynamic-derivations mode cannot accept
        `crateOverrides` as an inline value (it must cross a derivation
        boundary). Pass `crateOverridesFile = ./overrides.nix` instead,
        where the file evaluates to `pkgs: { <crateName> = attrs: { … }; }`.
      ''
    else if haveDynDrv then
      x: x
    else
      throw ''
        cargo-nix-plugin: no plugin loaded (`builtins.resolveCargoWorkspace`
        missing) and `builtins.outputOf` unavailable. Either load the
        plugin (`plugin-files = …/lib/nix/plugins`) or enable the
        dynamic-derivations mode:
          experimental-features = nix-command flakes ca-derivations dynamic-derivations
        and ensure builders have `system-features = recursive-nix`.
      '';

  manifestPath =
    if libArgs ? manifestPath && libArgs.manifestPath != null then
      libArgs.manifestPath
    else
      "${src}/Cargo.toml";
  lockPath = "${builtins.dirOf manifestPath}/Cargo.lock";

  # ---- workspace member enumeration (pure Nix) ---------------------------
  # The outer eval cannot ask the resolver which packages exist, so read
  # the workspace manifest directly. Mirrors cargo's [workspace.members]
  # semantics for the literal + single-level-glob cases; nested globs are
  # rejected with a clear error so users know to pin members explicitly.
  rootManifest = builtins.fromTOML (builtins.readFile manifestPath);
  hasWorkspaceTable = rootManifest ? workspace;

  expandMember =
    pat:
    let
      parts = lib.splitString "/" pat;
      hasGlob = lib.any (p: lib.hasInfix "*" p) parts;
      # Only support a trailing `dir/*` glob, the dominant pattern in real
      # workspaces. Anything fancier needs explicit member listing.
      prefixParts = lib.init parts;
      lastPart = lib.last parts;
      prefix = lib.concatStringsSep "/" prefixParts;
      dir = if prefix == "" then src else src + "/${prefix}";
      entries = builtins.readDir dir;
      matchingDirs = lib.filterAttrs (
        n: t: t == "directory" && builtins.pathExists (dir + "/${n}/Cargo.toml")
      ) entries;
    in
    if !hasGlob then
      [ pat ]
    else if lastPart == "*" && !lib.any (p: lib.hasInfix "*" p) prefixParts then
      map (n: if prefix == "" then n else "${prefix}/${n}") (lib.attrNames matchingDirs)
    else
      throw ''
        cargo-nix-plugin: dynamic-derivations mode cannot expand
        workspace member glob `${pat}`. Only trailing `dir/*` globs are
        supported — list members explicitly in [workspace.members] or use
        the plugin / resolvedJson path.
      '';

  excludes = lib.flatten (map expandMember (rootManifest.workspace.exclude or [ ]));
  declaredMembers = lib.flatten (map expandMember (rootManifest.workspace.members or [ ]));
  # The workspace root itself is a member when it has a [package] table.
  rootIsPkg = rootManifest ? package;
  memberDirs = lib.unique (
    (lib.optional rootIsPkg ".") ++ (lib.subtractLists excludes declaredMembers)
  );

  memberName =
    dir:
    let
      m = builtins.fromTOML (
        builtins.readFile (if dir == "." then manifestPath else "${src}/${dir}/Cargo.toml")
      );
    in
    m.package.name or (throw "cargo-nix-plugin: ${dir}/Cargo.toml has no [package.name]");

  workspaceMemberNames =
    if hasWorkspaceTable then
      map memberName memberDirs
    else
      # Single-package crate, no [workspace] table.
      [ rootManifest.package.name ];

  # ---- vendor + planner ---------------------------------------------------
  # Reuse nixpkgs' importCargoLock for offline cargo-metadata: gives a
  # vendor dir + .cargo/config.toml whose hashes all come from Cargo.lock,
  # so no extra checked-in artifact. allowBuiltinFetchGit matches how the
  # plugin path fetches git deps (builtins.fetchGit, no outputHashes).
  vendorDir = pkgs.rustPlatform.importCargoLock {
    # `lockPath` may be a context-carrying string (when `src` is a store
    # path) which can't be coerced to a Nix path. Pass contents instead.
    lockFileContents = builtins.readFile lockPath;
    allowBuiltinFetchGit = true;
  };

  cargoNixResolve = pkgs.callPackage ./cargo-nix-resolve.nix { };
  # The inner eval imports `lib/default.nix`, which in turn references
  # `../nix/build-rust-crate*` and `../builder/` by relative path — so
  # the whole repo tree must be one store path. Filter to the bits the
  # inner eval needs (excludes tests/ and rust/, which dwarf the rest).
  pluginRepo = builtins.path {
    name = "cargo-nix-plugin-lib";
    path = ../.;
    filter =
      p: t:
      let
        rel = lib.removePrefix (toString ../. + "/") (toString p);
        top = builtins.head (lib.splitString "/" rel);
      in
      lib.elem top [
        "lib"
        "nix"
        "builder"
      ];
  };
  evalNix = "${pluginRepo}/nix/dyn-drv-eval.nix";

  # Inner eval needs nixpkgs for buildRustCrate / stdenv. Pin the exact
  # source the caller is using so the planner produces drvs that match
  # what the plugin path would have produced.
  nixpkgsSrc = pkgs.path;
  system = stdenv.hostPlatform.system;

  # Args the inner eval consumes — everything serializable from libArgs
  # plus paths the planner makes available in its closure.
  innerArgsJson = builtins.toJSON {
    inherit
      apiLevel
      system
      target
      rootFeatures
      noDefaultFeatures
      clippyArgs
      extraCfgs
      ;
    nixpkgs = nixpkgsSrc;
    src = "${src}";
    libDir = "${pluginRepo}/lib";
    crateOverridesFile = if crateOverridesFile != null then "${crateOverridesFile}" else null;
    manifestPath = "${manifestPath}";
    extraRegistries = libArgs.extraRegistries or { };
  };

  # Pin the inner nix. Recursive-nix exposes the outer daemon socket;
  # the inner CLI needs to speak the protocol and have the experimental
  # features compiled in.
  innerNix = pkgs.nix;

  mkPlanner =
    segments:
    let
      slug = lib.replaceStrings [ "." "\"" ] [ "-" "" ] (lib.concatStringsSep "--" segments);
      # The CA text output's basename is the planner drv's `name`. The
      # inner eval renames its selected derivation to the same string so
      # Nix's drv-name-must-match-path invariant holds.
      innerDrvName = "cargo-nix-plan-${slug}";
    in
    pkgs.stdenvNoCC.mkDerivation {
      name = "${innerDrvName}.drv";
      requiredSystemFeatures = [ "recursive-nix" ];
      __contentAddressed = true;
      outputHashMode = "text";
      outputHashAlgo = "sha256";

      nativeBuildInputs = [
        pkgs.cargo
        pkgs.jq
        cargoNixResolve
        innerNix
      ];

      # Drv path inputs; unsafeDiscardOutputDependency so the planner only
      # needs the .drv text in its closure, not the realized outputs.
      env = {
        VENDOR_DIR = vendorDir;
        SRC = "${src}";
        EVAL_NIX = evalNix;
        INNER_ARGS = innerArgsJson;
        WS_ATTR_SEGMENTS = builtins.toJSON segments;
        WS_DRV_NAME = innerDrvName;
        TARGET_JSON = builtins.toJSON target;
        ROOT_FEATURES_JSON = builtins.toJSON rootFeatures;
        NO_DEFAULT_FEATURES = if noDefaultFeatures then "1" else "";
        CARGO_NET_OFFLINE = "true";
        # Inner nix must opt into the same features. recursive-nix is
        # implied by being inside a recursive-nix build.
        NIX_CONFIG = ''
          experimental-features = nix-command flakes ca-derivations dynamic-derivations
        '';
      };

      buildCommand = ''
        runHook preBuild

        export HOME=$TMPDIR
        mkdir -p .cargo
        # importCargoLock writes a *relative* `directory = "cargo-vendor-dir"`
        # (cargoSetupHook normally rewrites it after copying the vendor tree).
        # We point cargo straight at the store path instead.
        sed 's|directory = "cargo-vendor-dir"|directory = "'"$VENDOR_DIR"'"|' \
          "$VENDOR_DIR/.cargo/config.toml" > .cargo/config.toml

        # cargo metadata wants a writable target dir even with --offline.
        export CARGO_TARGET_DIR=$TMPDIR/target

        cargo metadata \
          --manifest-path "$SRC/Cargo.toml" \
          --format-version 1 \
          --offline --locked \
          > metadata.json

        # Build the PluginInput envelope cargo-nix-resolve expects.
        jq -n \
          --rawfile metadata metadata.json \
          --rawfile cargoLock "$SRC/Cargo.lock" \
          --argjson target "$TARGET_JSON" \
          --argjson rootFeatures "$ROOT_FEATURES_JSON" \
          --argjson noDefaultFeatures "''${NO_DEFAULT_FEATURES:+true}''${NO_DEFAULT_FEATURES:-false}" \
          '{metadata: $metadata, cargoLock: $cargoLock, target: $target,
            rootFeatures: $rootFeatures, noDefaultFeatures: $noDefaultFeatures}' \
          > input.json

        cargo-nix-resolve input.json > resolved.json

        # Re-enter lib/default.nix with the resolved graph; instantiate
        # the requested attr.
        echo "$INNER_ARGS" > args.json
        drv=$(nix-instantiate "$EVAL_NIX" \
          --arg argsFile "\"$PWD/args.json\"" \
          --arg resolvedJson "\"$PWD/resolved.json\"" \
          --argstr attrSegments "$WS_ATTR_SEGMENTS" \
          --argstr drvName "$WS_DRV_NAME")

        cp "$drv" $out
        runHook postBuild
      '';
    };

  # outputOf yields a placeholder string, not a derivation. Wrap it so
  # callers get something `nix build` can target and `meta`/`name` exist.
  wrap =
    name: segments:
    let
      planner = mkPlanner segments;
      # planner is a CA text-output drv whose `out` is a `.drv` file.
      # outputOf turns that into a deferred placeholder for *its* output.
      placeholder = builtins.outputOf planner.outPath "out";
    in
    pkgs.runCommand name { passthru = { inherit planner; }; } ''
      ln -s ${placeholder} $out
    '';

  mkMember = name: {
    build = wrap "${name}" [
      "workspaceMembers"
      name
      "build"
    ];
    buildTests = wrap "${name}-tests" [
      "workspaceMembers"
      name
      "buildTests"
    ];
    runTests = wrap "${name}-run-tests" [
      "workspaceMembers"
      name
      "runTests"
    ];
  };
in
guard {
  workspaceMembers = lib.genAttrs workspaceMemberNames mkMember;

  allWorkspaceMembers = pkgs.symlinkJoin {
    name = "all-workspace-members";
    paths = map (n: (mkMember n).build) workspaceMemberNames;
  };

  rootCrate =
    if rootIsPkg || !hasWorkspaceTable then
      {
        build = wrap rootManifest.package.name [
          "rootCrate"
          "build"
        ];
      }
    else
      null;

  clippy =
    let
      clippyMember =
        name:
        wrap "${name}-clippy" [
          "clippy"
          "workspaceMembers"
          name
          "build"
        ];
    in
    {
      workspaceMembers = lib.genAttrs workspaceMemberNames (name: {
        build = clippyMember name;
      });
      allWorkspaceMembers = pkgs.symlinkJoin {
        name = "all-workspace-members-clippy";
        paths = map clippyMember workspaceMemberNames;
      };
    };

  inherit apiLevel;
  resolverApiLevel = 0;
  resolveMode = "dyn-drv";
}
