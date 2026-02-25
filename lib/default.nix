# Nix wrapper that connects the cargo-nix-plugin output to buildRustCrate.
#
# Usage:
#   let
#     cargoNix = import ./lib {
#       inherit pkgs;
#       metadata = builtins.readFile ./metadata.json;
#       cargoLock = builtins.readFile ./Cargo.lock;
#       src = ./.;  # workspace root
#     };
#   in cargoNix.workspaceMembers

{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  stdenv ? pkgs.stdenv,
  # Required: output of `cargo metadata --format-version 1 --locked`
  metadata ? null,
  # Required: contents of Cargo.lock
  cargoLock ? null,
  # Required: workspace source root
  src ? null,
  # Optional: function to create buildRustCrate for a given pkgs
  buildRustCrateForPkgs ? pkgs: pkgs.buildRustCrate,
  # Optional: default crate overrides
  defaultCrateOverrides ? pkgs.defaultCrateOverrides,
  # Optional: features to enable
  rootFeatures ? [ "default" ],
  # Optional: target platform description (auto-detected from stdenv)
  target ? null,
}:

let
  # Build the target description from stdenv if not provided
  defaultTarget = makeDefaultTarget stdenv.hostPlatform;

  makeDefaultTarget =
    platform:
    {
      name = platform.rust.rustcTargetSpec or platform.rust.rustcTarget or "x86_64-unknown-linux-gnu";
      os =
        if platform.isLinux then
          "linux"
        else if platform.isDarwin then
          "macos"
        else if platform.isWindows then
          "windows"
        else if platform.isFreeBSD then
          "freebsd"
        else
          "unknown";
      arch =
        if platform.isx86_64 then
          "x86_64"
        else if platform.isAarch64 then
          "aarch64"
        else if platform.isi686 then
          "i686"
        else if platform.isAarch32 then
          "arm"
        else if platform.isRiscV64 then
          "riscv64"
        else
          "unknown";
      vendor =
        if platform.isLinux then
          "unknown"
        else if platform.isDarwin then
          "apple"
        else
          "unknown";
      env =
        if platform.isLinux && platform.isGnu then
          "gnu"
        else if platform.isLinux && platform.isMusl then
          "musl"
        else
          "";
      family =
        if platform.isUnix then [ "unix" ] else if platform.isWindows then [ "windows" ] else [ ];
      pointer_width =
        if platform.is64bit then "64" else if platform.is32bit then "32" else "64";
      endian = if platform.isLittleEndian then "little" else "big";
      unix = platform.isUnix;
      windows = platform.isWindows;
    };

  resolvedTarget = if target != null then target else defaultTarget;

  # Call the plugin builtin
  resolved = builtins.resolveCargoWorkspace {
    inherit metadata cargoLock;
    target = resolvedTarget;
    inherit rootFeatures;
  };

  # Source resolution: given a crate's source info, produce a src path
  resolveSrc =
    crateInfo:
    if crateInfo.source == null then
      null
    else if crateInfo.source.type == "local" then
      # Filter source to just this crate's directory relative to workspace root
      let
        # The path is absolute from cargo metadata; we need it relative to src
        # For now, use src directly (workspace members share the workspace src)
      in
      src
    else if crateInfo.source.type == "crates-io" then
      null # buildRustCrate handles fetching via sha256
    else if crateInfo.source.type == "git" then
      builtins.fetchGit {
        url = crateInfo.source.url;
        rev = crateInfo.source.rev;
      }
    else
      null;

  # Build a crate using buildRustCrate
  # Memoization via the `self` pattern (builtByPackageId)
  mkBuiltByPackageIdByPkgs =
    cratePkgs:
    let
      buildRustCrate =
        let
          base = buildRustCrateForPkgs cratePkgs;
        in
        if defaultCrateOverrides != pkgs.defaultCrateOverrides then
          base.override { defaultCrateOverrides = defaultCrateOverrides; }
        else
          base;

      self = {
        crates = lib.mapAttrs (packageId: _: buildCrate self cratePkgs buildRustCrate packageId) resolved.crates;
        target = makeDefaultTarget cratePkgs.stdenv.hostPlatform;
        build = mkBuiltByPackageIdByPkgs cratePkgs.buildPackages;
      };
    in
    self;

  buildCrate =
    self: cratePkgs: buildRustCrate: packageId:
    let
      crateInfo = resolved.crates.${packageId};

      # Wire dependencies as built derivations
      mapDeps =
        deps:
        map (
          dep:
          let
            depCrateInfo = resolved.crates.${dep.packageId} or null;
            # proc-macro crates must be built for the build platform
            built =
              if depCrateInfo != null && depCrateInfo.procMacro then
                self.build.crates.${dep.packageId}
              else
                self.crates.${dep.packageId};
          in
          {
            inherit (dep) name;
            drv = built;
            rename = dep.rename or null;
          }
        ) deps;

      dependencies = mapDeps (crateInfo.dependencies or [ ]);
      buildDependencies = mapDeps (crateInfo.buildDependencies or [ ]);

      crateSrc = resolveSrc crateInfo;
    in
    buildRustCrate ({
        crateName = crateInfo.crateName;
        version = crateInfo.version;
        edition = crateInfo.edition;
        sha256 = crateInfo.sha256;
        src = crateSrc;
        authors = crateInfo.authors or [ ];
        dependencies = dependencies;
        buildDependencies = buildDependencies;
        features = crateInfo.resolvedDefaultFeatures or [ ];
        build = crateInfo.build;
        libPath = crateInfo.libPath or null;
        libName = crateInfo.libName or null;
        procMacro = crateInfo.procMacro or false;
        links = crateInfo.links or null;
        crateBin = crateInfo.crateBin or [ ];
      }
      // lib.optionalAttrs (crateInfo.libCrateTypes or [ ] != [ ]) {
        type = crateInfo.libCrateTypes;
      });

  builtCrates = mkBuiltByPackageIdByPkgs pkgs;

in
{
  # Public interface matching crate2nix
  workspaceMembers = lib.mapAttrs (
    name: packageId: {
      inherit packageId;
      build = builtCrates.crates.${packageId};
    }
  ) resolved.workspaceMembers;

  rootCrate =
    if resolved.root != null then
      {
        packageId = resolved.root;
        build = builtCrates.crates.${resolved.root};
      }
    else
      null;

  allWorkspaceMembers = pkgs.symlinkJoin {
    name = "all-workspace-members";
    paths = lib.mapAttrsToList (_name: packageId: builtCrates.crates.${packageId}) resolved.workspaceMembers;
  };

  # Expose internals for debugging
  inherit resolved;
  inherit builtCrates;
}
